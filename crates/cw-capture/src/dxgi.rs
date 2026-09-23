//! DXGI Desktop Duplication backend, on `windows-capture`.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use windows::Win32::Graphics::Gdi::HMONITOR;
use windows_capture::d3d11::StagingTexture;
use windows_capture::dxgi_duplication_api::{
    DxgiDuplicationApi, DxgiDuplicationFormat, Error as DuplicationError,
};
use windows_capture::monitor::Monitor;

use crate::{CaptureError, Capturer, Frame, MonitorInfo, Recoverable, enumerate_monitors};

/// How long each acquire waits for a desktop update. The duplication accumulates updates between
/// ticks, so a pending frame is normally returned at once.
const ACQUIRE_TIMEOUT_MS: u32 = 100;

/// Sessions are persistent: recreating the duplication per capture is what ruled out the
/// alternatives in the backend benchmark.
pub(crate) struct DxgiCapturer {
    /// Refreshed by `monitors`, which is how a hotplugged monitor gets a session and a detached
    /// one loses it.
    known: Vec<(HMONITOR, MonitorInfo)>,
    sessions: HashMap<String, Session>,
    /// Skip reasons already warned about, held for `enumerate_monitors`'s deduplication.
    skips: HashMap<isize, String>,
}

impl DxgiCapturer {
    pub(crate) fn new() -> Self {
        Self {
            known: Vec::new(),
            sessions: HashMap::new(),
            skips: HashMap::new(),
        }
    }
}

impl Capturer for DxgiCapturer {
    fn monitors(&mut self) -> Result<Vec<MonitorInfo>, CaptureError> {
        self.known = enumerate_monitors(&mut self.skips)?;
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

struct Session {
    duplication: DxgiDuplicationApi,
    staging: Option<StagingTexture>,
}

impl Session {
    fn open(handle: HMONITOR) -> Result<Self, CaptureError> {
        let duplication = DxgiDuplicationApi::new_options(
            Monitor::from_raw_hmonitor(handle.0),
            &[DxgiDuplicationFormat::Bgra8],
        )
        .map_err(classify)?;
        Ok(Self {
            duplication,
            staging: None,
        })
    }

    fn capture(&mut self, monitor_id: &str, dpi_scale: f32) -> Result<Frame, CaptureError> {
        let Self {
            duplication,
            staging,
        } = self;

        for _ in 0..2 {
            let mut frame = duplication
                .acquire_next_frame(ACQUIRE_TIMEOUT_MS)
                .map_err(classify)?;
            // A pointer-only update carries no desktop image, and the first frame of a session can
            // carry none either. `LastPresentTime` is how the API says so.
            if frame.frame_info().LastPresentTime == 0 {
                continue;
            }

            let desc = *frame.texture_desc();
            let staging = match staging {
                Some(texture)
                    if texture.desc().Width == desc.Width
                        && texture.desc().Height == desc.Height
                        && texture.desc().Format == desc.Format =>
                {
                    texture
                }
                slot => slot.insert(
                    StagingTexture::new(frame.device(), desc.Width, desc.Height, desc.Format)
                        .map_err(|error| Recoverable::DeviceLost(error.to_string()))?,
                ),
            };

            let buffer = frame.buffer_with(staging).map_err(classify)?;
            let (width, height) = (buffer.width(), buffer.height());
            let format = buffer.format();
            let mut packed = Vec::new();
            let mut bgra = buffer.as_nopadding_buffer(&mut packed).to_vec();
            match format {
                DxgiDuplicationFormat::Bgra8 => {}
                DxgiDuplicationFormat::Rgba8 => {
                    for pixel in bgra.as_chunks_mut::<4>().0 {
                        pixel.swap(0, 2);
                    }
                }
                other => return Err(Recoverable::UnsupportedFormat(format!("{other:?}")).into()),
            }
            for pixel in bgra.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }

            return Ok(Frame {
                monitor_id: monitor_id.to_owned(),
                width,
                height,
                dpi_scale,
                bgra,
                captured_at: chrono::Utc::now(),
            });
        }

        Err(Recoverable::NoNewFrame.into())
    }
}

fn classify(error: DuplicationError) -> CaptureError {
    match error {
        DuplicationError::Timeout => Recoverable::NoNewFrame.into(),
        DuplicationError::AccessLost => Recoverable::AccessLost.into(),
        DuplicationError::OutputNotFound => Recoverable::MonitorGone.into(),
        other => Recoverable::DeviceLost(other.to_string()).into(),
    }
}
