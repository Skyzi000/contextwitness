//! Windows Graphics Capture fallback, on `windows-capture`.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::{D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::d3d11::StagingTexture;
use windows_capture::frame::Frame as WgcFrame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::{
    CaptureError, Capturer, Frame, MonitorInfo, Recoverable, STALE_SHOT_SECONDS, enumerate_monitors,
};

/// WGC pushes a frame per screen update, up to the refresh rate, and a readback copies the full
/// screen.
const MIN_READBACK_INTERVAL: Duration = Duration::from_millis(500);

/// How long a fresh session may stay silent before it is called broken instead of idle. After the
/// first frame, silence is just an unchanging screen and says nothing about the session.
const FIRST_FRAME_GRACE: Duration = Duration::from_secs(5);

type Mailbox = Arc<Mutex<Option<Shot>>>;
type Control = CaptureControl<Sink, <Sink as GraphicsCaptureApiHandler>::Error>;

pub(crate) struct WgcCapturer {
    known: Vec<(HMONITOR, MonitorInfo)>,
    sessions: HashMap<String, Session>,
}

impl WgcCapturer {
    pub(crate) fn new() -> Self {
        Self {
            known: Vec::new(),
            sessions: HashMap::new(),
        }
    }

    /// Drop a monitor's session.
    pub(crate) fn release(&mut self, monitor_id: &str) {
        self.sessions.remove(monitor_id);
    }

    /// Drop sessions for monitors that are gone. `monitors` does this too, but only when something
    /// actually captures through this backend: were the last fallback monitor detached, nothing
    /// would call in again and its capture thread, D3D device and mailbox would outlive it by the
    /// life of the process.
    pub(crate) fn retain_monitors(&mut self, monitors: &[MonitorInfo]) {
        self.sessions
            .retain(|id, _| monitors.iter().any(|monitor| &monitor.id == id));
    }

    /// Drop every session. A frame from a session alive before this call can sit in a mailbox or
    /// wait in a frame pool arbitrarily long; it can never reach a later capture.
    pub(crate) fn discard_pending(&mut self) {
        self.sessions.clear();
    }
}

impl Capturer for WgcCapturer {
    fn monitors(&mut self) -> Result<Vec<MonitorInfo>, CaptureError> {
        self.known = enumerate_monitors();
        let known = &self.known;
        self.sessions
            .retain(|id, _| known.iter().any(|(_, monitor)| &monitor.id == id));
        Ok(self
            .known
            .iter()
            .map(|(_, monitor)| monitor.clone())
            .collect())
    }

    fn capture(&mut self, monitor_id: &str) -> Result<Frame, CaptureError> {
        self.monitors()?;
        let (handle, dpi_scale) = self
            .known
            .iter()
            .find(|(_, monitor)| monitor.id == monitor_id)
            .map(|(handle, monitor)| (*handle, monitor.dpi_scale))
            .ok_or(Recoverable::MonitorGone)?;

        let session = match self.sessions.entry(monitor_id.to_owned()) {
            Entry::Occupied(occupied) => occupied.into_mut(),
            Entry::Vacant(vacant) => vacant.insert(Session::open(handle)?),
        };

        let captured = session.capture(monitor_id, dpi_scale);
        if matches!(&captured, Err(error) if !matches!(error, CaptureError::Recoverable(Recoverable::NoNewFrame)))
        {
            self.sessions.remove(monitor_id);
        }
        captured
    }
}

/// One monitor's frame, as the callback thread leaves it for the tick thread.
struct Shot {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
    captured_at: chrono::DateTime<chrono::Utc>,
}

/// A live capture thread plus the mailbox it writes into.
struct Session {
    control: Option<Control>,
    mailbox: Mailbox,
    opened: Instant,
    delivered: bool,
}

impl Session {
    fn open(handle: HMONITOR) -> Result<Self, CaptureError> {
        let mailbox: Mailbox = Arc::new(Mutex::new(None));
        // Asking for a borderless capture where the OS does not support it costs the whole session.
        let border = if GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false) {
            DrawBorderSettings::WithoutBorder
        } else {
            DrawBorderSettings::Default
        };
        let settings = Settings::new(
            Monitor::from_raw_hmonitor(handle.0),
            // The duplication never composes the cursor; a fallback that did would change what
            // the record means depending on which backend answered.
            CursorCaptureSettings::WithoutCursor,
            border,
            SecondaryWindowSettings::Default,
            // The session-level interval needs an API only the newest Windows builds have and
            // fails the session on the rest; frames are thinned in the callback instead.
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            mailbox.clone(),
        );
        let control = Sink::start_free_threaded(settings)
            .map_err(|error| Recoverable::DeviceLost(error.to_string()))?;
        Ok(Self {
            control: Some(control),
            mailbox,
            opened: Instant::now(),
            delivered: false,
        })
    }

    fn capture(&mut self, monitor_id: &str, dpi_scale: f32) -> Result<Frame, CaptureError> {
        if self
            .control
            .as_ref()
            .is_none_or(|control| control.is_finished())
        {
            return Err(Recoverable::AccessLost.into());
        }

        let shot = lock(&self.mailbox).take();
        match shot {
            Some(shot) => {
                self.delivered = true;
                Ok(Frame {
                    monitor_id: monitor_id.to_owned(),
                    width: shot.width,
                    height: shot.height,
                    dpi_scale,
                    bgra: shot.bgra,
                    captured_at: shot.captured_at,
                })
            }
            None if !self.delivered && self.opened.elapsed() > FIRST_FRAME_GRACE => {
                Err(Recoverable::AccessLost.into())
            }
            None => Err(Recoverable::NoNewFrame.into()),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Dropping the control leaves its thread and its D3D device running forever.
        if let Some(control) = self.control.take() {
            let _ = control.stop();
        }
    }
}

/// The capture-thread half: it holds no state the tick thread reads except the mailbox.
struct Sink {
    mailbox: Mailbox,
    last_readback: Option<Instant>,
}

impl GraphicsCaptureApiHandler for Sink {
    type Flags = Mailbox;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            mailbox: ctx.flags,
            last_readback: None,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut WgcFrame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // Stamped before anything else, so the wait for the readback is never inside the stamp.
        let arrived = Instant::now();
        let captured_at = chrono::Utc::now();
        if self
            .last_readback
            .is_some_and(|at| at.elapsed() < MIN_READBACK_INTERVAL)
        {
            return Ok(());
        }

        // Hand-rolled instead of `Frame::buffer`: that one hands back a slice into a mapping it
        // has already released. A new texture per call because `start_free_threaded` requires the
        // handler to be `Send` and a D3D texture is not.
        let (width, height) = (frame.width(), frame.height());
        let staging = StagingTexture::new(frame.device(), width, height, frame.desc().Format)?;
        let context = frame.device_context();
        let row = width as usize * 4;
        let mut bgra = vec![0u8; row * height as usize];
        unsafe {
            context.CopyResource(staging.texture(), frame.as_raw_texture());
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(staging.texture(), 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            for y in 0..height as usize {
                std::ptr::copy_nonoverlapping(
                    mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize),
                    bgra.as_mut_ptr().add(y * row),
                    row,
                );
            }
            context.Unmap(staging.texture(), 0);
        }
        for pixel in bgra.chunks_exact_mut(4) {
            pixel[3] = 255;
        }

        // Keep the freshness verdict and the post atomic with respect to a pull.
        // `Instant` (QPC) is the basis for elapsed time including standby and hibernate; UTC is an
        // additional guard. Backward wall-clock corrections are outside the guarantee.
        let mut mailbox = lock(&self.mailbox);
        let now = Instant::now();
        let now_utc = chrono::Utc::now();
        let stale_after = Duration::from_secs(STALE_SHOT_SECONDS);
        let wall_elapsed = if now_utc >= captured_at {
            now_utc - captured_at
        } else {
            chrono::TimeDelta::zero()
        };
        let stale = now.duration_since(arrived) > stale_after
            || wall_elapsed > chrono::TimeDelta::seconds(STALE_SHOT_SECONDS as i64);
        if stale {
            drop(mailbox);
            // The Err ends the capture thread; once it has ended, a pull sees `AccessLost`, drops
            // the session, and a later pull opens a fresh one.
            return Err(format!(
                "stale WGC callback exceeded the {STALE_SHOT_SECONDS}-second post allowance"
            )
            .into());
        }

        self.last_readback = Some(now);
        *mailbox = Some(Shot {
            width,
            height,
            bgra,
            captured_at,
        });
        Ok(())
    }
}

/// A poisoned guard still holds a usable mailbox; panicking here would take the capture thread with
/// it and prevent the tick thread from recovering the value.
fn lock(mailbox: &Mailbox) -> std::sync::MutexGuard<'_, Option<Shot>> {
    mailbox.lock().unwrap_or_else(PoisonError::into_inner)
}
