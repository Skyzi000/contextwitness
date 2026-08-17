//! Windows Graphics Capture fallback, on `windows-capture`.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::{
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
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

/// How long a fresh session may stay silent before it is called broken instead of idle.
const FIRST_FRAME_GRACE: Duration = Duration::from_secs(5);

type Control = CaptureControl<Sink, <Sink as GraphicsCaptureApiHandler>::Error>;

pub(crate) struct WgcCapturer {
    known: Vec<(HMONITOR, MonitorInfo)>,
    sessions: HashMap<String, Session>,
    /// Skip reasons already warned about, held for `enumerate_monitors`'s deduplication.
    skips: HashMap<isize, String>,
    /// Delivery floor set by `discard_pending`; sessions opened after it refuse older stamps.
    floor_100ns: i64,
}

impl WgcCapturer {
    pub(crate) fn new() -> Self {
        Self {
            known: Vec::new(),
            sessions: HashMap::new(),
            skips: HashMap::new(),
            floor_100ns: 0,
        }
    }

    /// Drop a monitor's session.
    pub(crate) fn release(&mut self, monitor_id: &str) {
        self.sessions.remove(monitor_id);
    }

    /// Drop sessions for monitors that are gone. `monitors` does this too, but only when something
    /// actually captures through this backend: were the last fallback monitor detached, nothing
    /// would call in again and its capture thread and D3D device would outlive it by the life of
    /// the process.
    pub(crate) fn retain_monitors(&mut self, monitors: &[MonitorInfo]) {
        self.sessions
            .retain(|id, _| monitors.iter().any(|monitor| &monitor.id == id));
    }

    /// Drop every session and floor later deliveries above this instant. Destroying the sessions
    /// disposes every held texture and queued frame; the floor covers what destruction cannot —
    /// nothing documents that a fresh session's first compose postdates its creation, so the
    /// boundary is enforced on the composition stamp instead of assumed of the pipeline.
    pub(crate) fn discard_pending(&mut self) {
        self.sessions.clear();
        // Refusing every capture forever is the correct answer to a clock that cannot be read.
        self.floor_100ns = qpc_now_100ns().unwrap_or(i64::MAX);
    }
}

impl Capturer for WgcCapturer {
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
        self.monitors()?;
        let (handle, dpi_scale) = self
            .known
            .iter()
            .find(|(_, monitor)| monitor.id == monitor_id)
            .map(|(handle, monitor)| (*handle, monitor.dpi_scale))
            .ok_or(Recoverable::MonitorGone)?;

        let floor_100ns = self.floor_100ns;
        let session = match self.sessions.entry(monitor_id.to_owned()) {
            Entry::Occupied(occupied) => occupied.into_mut(),
            Entry::Vacant(vacant) => vacant.insert(Session::open(handle, floor_100ns)?),
        };

        let captured = session.capture(monitor_id, dpi_scale);
        if matches!(&captured, Err(error) if !matches!(error, CaptureError::Recoverable(Recoverable::NoNewFrame)))
        {
            self.sessions.remove(monitor_id);
        }
        captured
    }
}

/// A live capture thread plus the sink it feeds; the sink is reached through the wrapper's
/// callback mutex.
struct Session {
    control: Option<Control>,
    opened: Instant,
    delivered: bool,
}

impl Session {
    fn open(handle: HMONITOR, floor_100ns: i64) -> Result<Self, CaptureError> {
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
            // No session-level interval even where the OS has one: nothing documents what the
            // compositor does with a frame it withholds, and a withheld final repaint would be
            // the silent loss this sink exists to prevent. The guarantee is enforced instead by
            // landing every handled arrival.
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            floor_100ns,
        );
        let control = Sink::start_free_threaded(settings)
            .map_err(|error| Recoverable::DeviceLost(error.to_string()))?;
        Ok(Self {
            control: Some(control),
            opened: Instant::now(),
            delivered: false,
        })
    }

    fn capture(&mut self, monitor_id: &str, dpi_scale: f32) -> Result<Frame, CaptureError> {
        let Some(control) = self
            .control
            .as_ref()
            .filter(|control| !control.is_finished())
        else {
            return Err(Recoverable::AccessLost.into());
        };
        let callback = control.callback();
        let pulled = callback
            .lock()
            .pull()
            .map_err(|error| Recoverable::DeviceLost(error.to_string()))?;
        match pulled {
            Pulled::Delivered(width, height, bgra, captured_at) => {
                self.delivered = true;
                Ok(Frame {
                    monitor_id: monitor_id.to_owned(),
                    width,
                    height,
                    dpi_scale,
                    bgra,
                    captured_at,
                })
            }
            Pulled::Stale => Err(Recoverable::StaleFrame.into()),
            Pulled::Nothing => Err(verdict(self.delivered, self.opened.elapsed())),
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

/// An idle screen is only idle behind a session that has delivered once; a session that has
/// delivered nothing through its whole grace is broken.
fn verdict(delivered: bool, open_for: Duration) -> CaptureError {
    if !delivered && open_for > FIRST_FRAME_GRACE {
        Recoverable::AccessLost.into()
    } else {
        Recoverable::NoNewFrame.into()
    }
}

/// `SystemRelativeTime`'s unit — 100 ns ticks on the QPC time base. A `QueryPerformanceCounter`
/// reading ticks at `QueryPerformanceFrequency` and must be converted into this unit.
const HUNDRED_NS_PER_SECOND: i64 = 10_000_000;

/// Which arrival generation the tick has delivered, and when the held content was composed —
/// raw `SystemRelativeTime` ticks plus a wall estimate for the delivery stamp.
#[derive(Default)]
struct Freshness {
    generation: u64,
    delivered: u64,
    arrived: Option<(i64, chrono::DateTime<chrono::Utc>)>,
}

/// What a judgement of the held frame allows.
#[derive(Debug, PartialEq)]
enum Delivery {
    /// Nothing arrived since the last delivery.
    Nothing,
    /// The undelivered hold is past the stale bound on a clock, or stamped at or before the
    /// discard floor; only a rebuilt session can compose anew.
    Stale,
    /// Deliverable, stamped with the wall estimate of its composition.
    Fresh(chrono::DateTime<chrono::Utc>),
}

impl Freshness {
    fn arrive(&mut self, composed_100ns: i64, wall: chrono::DateTime<chrono::Utc>) {
        self.generation += 1;
        self.arrived = Some((composed_100ns, wall));
    }

    /// The age is the composition stamp against a pull-time reading of the same counter — QPC
    /// counts standby and hibernate, so a suspend lands in the age wherever it falls. UTC is an
    /// additional guard; backward wall-clock corrections are outside the guarantee. The floor is
    /// the discard boundary: a stamp at or before it is refused however young it is.
    fn deliverable(
        &self,
        now_100ns: i64,
        wall_now: chrono::DateTime<chrono::Utc>,
        floor_100ns: i64,
    ) -> Delivery {
        if self.generation == self.delivered {
            return Delivery::Nothing;
        }
        let Some((composed_100ns, wall)) = self.arrived else {
            return Delivery::Nothing;
        };
        let wall_elapsed = if wall_now >= wall {
            wall_now - wall
        } else {
            chrono::TimeDelta::zero()
        };
        let fresh = now_100ns - composed_100ns <= STALE_SHOT_SECONDS as i64 * HUNDRED_NS_PER_SECOND
            && composed_100ns > floor_100ns
            && wall_elapsed <= chrono::TimeDelta::seconds(STALE_SHOT_SECONDS as i64);
        if fresh {
            Delivery::Fresh(wall)
        } else {
            Delivery::Stale
        }
    }

    fn mark_delivered(&mut self) {
        self.delivered = self.generation;
    }
}

/// The capture-thread half. The newest arrival is kept, and kept on the GPU as one
/// `CopyResource` into a reused texture. The tick thread reads the held texture back through
/// the wrapper's callback mutex — the same lock `FrameArrived` holds — which also serializes
/// every use of the session's single-threaded D3D11 immediate context.
struct Sink {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    held: Option<(ID3D11Texture2D, D3D11_TEXTURE2D_DESC)>,
    staging: Option<StagingTexture>,
    freshness: Freshness,
    /// The discard boundary this session was opened under; kept out of `Freshness` so no reset
    /// of the arrival state can widen the discard's promise.
    floor_100ns: i64,
}

/// A reading of the counter `SystemRelativeTime` is stamped on, in the same 100-nanosecond
/// units, so ages need no second clock and no conversion race.
fn qpc_now_100ns() -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    let (mut counter, mut frequency) = (0i64, 0i64);
    unsafe {
        QueryPerformanceCounter(&mut counter)?;
        QueryPerformanceFrequency(&mut frequency)?;
    }
    Ok((i128::from(counter) * i128::from(HUNDRED_NS_PER_SECOND) / i128::from(frequency)) as i64)
}

impl GraphicsCaptureApiHandler for Sink {
    type Flags = i64;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            device: ctx.device,
            context: ctx.device_context,
            held: None,
            staging: None,
            freshness: Freshness::default(),
            floor_100ns: ctx.flags,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut WgcFrame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let landed = self.land(frame);
        if landed.is_err() {
            // The error halts the capture thread, but a tick already past its liveness check can
            // still pull before it sees the halt, so an arrival that fails here leaves nothing
            // behind. A failure upstream in the capture crate never reaches this sink; the hold
            // it strands stays behind the freshness rungs, which is all the bound available there.
            self.held = None;
            self.freshness = Freshness::default();
        }
        landed
    }
}

impl Sink {
    fn land(
        &mut self,
        frame: &mut WgcFrame,
    ) -> Result<(), <Self as GraphicsCaptureApiHandler>::Error> {
        // Stamps come before the copy: pixels landing in the held texture without a stamp of
        // their own would be delivered under the previous arrival's.
        let composed_100ns = frame.timestamp()?.Duration;
        let age_100ns = qpc_now_100ns()?.saturating_sub(composed_100ns).max(0) as u64;
        let age = Duration::new(
            age_100ns / HUNDRED_NS_PER_SECOND as u64,
            (age_100ns % HUNDRED_NS_PER_SECOND as u64) as u32 * 100,
        );
        // The wall estimate shares the age read: a suspend splitting these two adjacent reads
        // makes the label late by the suspend, up to the stale allowance — the raw ticks still
        // measure the true age, so the pixels stay inside the bound and only the label drifts;
        // a longer suspend is refused on the QPC rung.
        let Some(wall) = chrono::TimeDelta::from_std(age)
            .ok()
            .map(|delta| chrono::Utc::now() - delta)
        else {
            return Ok(());
        };
        let source = frame.desc();
        if !matches!(&self.held, Some((_, desc))
            if desc.Width == source.Width
                && desc.Height == source.Height
                && desc.Format == source.Format)
        {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: source.Width,
                Height: source.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: source.Format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: 0,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut texture = None;
            unsafe {
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut texture))?;
            }
            let texture = texture.ok_or("held texture creation returned nothing")?;
            self.held = Some((texture, desc));
        }
        let Some((held, _)) = self.held.as_ref() else {
            return Err("held texture missing after ensure".into());
        };
        unsafe {
            self.context.CopyResource(held, frame.as_raw_texture());
        }
        self.freshness.arrive(composed_100ns, wall);
        Ok(())
    }
}

/// What a pull found under the callback mutex.
enum Pulled {
    /// One readback: width, height, BGRA, estimated composition wall time.
    Delivered(u32, u32, Vec<u8>, chrono::DateTime<chrono::Utc>),
    /// Nothing new since the last delivery.
    Nothing,
    /// The undelivered hold outlived the stale bound; the session must be torn down.
    Stale,
}

impl Sink {
    /// One staging readback of the held texture, if anything arrived since the last delivery.
    fn pull(&mut self) -> Result<Pulled, <Self as GraphicsCaptureApiHandler>::Error> {
        let captured_at =
            match self
                .freshness
                .deliverable(qpc_now_100ns()?, chrono::Utc::now(), self.floor_100ns)
            {
                Delivery::Nothing => return Ok(Pulled::Nothing),
                Delivery::Stale => return Ok(Pulled::Stale),
                Delivery::Fresh(wall) => wall,
            };
        let Some((held, desc)) = self.held.as_ref() else {
            return Ok(Pulled::Nothing);
        };
        if !matches!(&self.staging, Some(staging)
            if staging.desc().Width == desc.Width
                && staging.desc().Height == desc.Height
                && staging.desc().Format == desc.Format)
        {
            self.staging = Some(StagingTexture::new(
                &self.device,
                desc.Width,
                desc.Height,
                desc.Format,
            )?);
        }
        let Some(staging) = self.staging.as_ref() else {
            return Err("staging texture missing after ensure".into());
        };

        // Hand-rolled instead of `Frame::buffer`: that one hands back a slice into a mapping it
        // has already released.
        let (width, height) = (desc.Width, desc.Height);
        let row = width as usize * 4;
        let mut bgra = vec![0u8; row * height as usize];
        unsafe {
            self.context.CopyResource(staging.texture(), held);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(staging.texture(), 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            for y in 0..height as usize {
                std::ptr::copy_nonoverlapping(
                    mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize),
                    bgra.as_mut_ptr().add(y * row),
                    row,
                );
            }
            self.context.Unmap(staging.texture(), 0);
        }
        for pixel in bgra.chunks_exact_mut(4) {
            pixel[3] = 255;
        }

        // A suspend can land inside the readback, so the bound is judged again after it; a
        // delivery is then still what the first judgement promised.
        match self
            .freshness
            .deliverable(qpc_now_100ns()?, chrono::Utc::now(), self.floor_100ns)
        {
            Delivery::Fresh(_) => {}
            Delivery::Nothing => return Ok(Pulled::Nothing),
            Delivery::Stale => return Ok(Pulled::Stale),
        }
        self.freshness.mark_delivered();
        Ok(Pulled::Delivered(width, height, bgra, captured_at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An arbitrary boot-relative composition instant, in `SystemRelativeTime`'s 100 ns ticks.
    const COMPOSED: i64 = 1_000 * HUNDRED_NS_PER_SECOND;
    const STALE_BOUND_100NS: i64 = STALE_SHOT_SECONDS as i64 * HUNDRED_NS_PER_SECOND;

    fn arrived() -> (Freshness, i64, chrono::DateTime<chrono::Utc>) {
        let mut freshness = Freshness::default();
        let wall = chrono::Utc::now();
        freshness.arrive(COMPOSED, wall);
        (freshness, COMPOSED, wall)
    }

    #[test]
    fn nothing_arrived_is_not_deliverable() {
        assert_eq!(
            Freshness::default().deliverable(COMPOSED, chrono::Utc::now(), 0),
            Delivery::Nothing
        );
    }

    #[test]
    fn a_fresh_arrival_is_deliverable_once() {
        let (mut freshness, at, wall) = arrived();
        // A pull later than the arrival must deliver the arrival's stamp, not its own.
        assert_eq!(
            freshness.deliverable(at, wall + chrono::TimeDelta::seconds(1), 0),
            Delivery::Fresh(wall)
        );
        freshness.mark_delivered();
        assert_eq!(freshness.deliverable(at, wall, 0), Delivery::Nothing);
    }

    #[test]
    fn one_delivery_clears_every_arrival_up_to_it() {
        // Two arrivals, one delivery: the pull consumes the whole backlog, not one arrival of it.
        let (mut freshness, at, wall) = arrived();
        let second_at = at + HUNDRED_NS_PER_SECOND;
        let second_wall = wall + chrono::TimeDelta::seconds(1);
        freshness.arrive(second_at, second_wall);
        freshness.mark_delivered();
        assert_eq!(
            freshness.deliverable(second_at, second_wall, 0),
            Delivery::Nothing
        );
    }

    #[test]
    fn a_later_arrival_reopens_delivery() {
        // The second arrival lands past the stale bound of the first, so a `Freshness` that kept
        // the first stamp would refuse delivery instead of handing back the new stamp.
        let (mut freshness, at, wall) = arrived();
        freshness.mark_delivered();
        let late_at = at + STALE_BOUND_100NS + HUNDRED_NS_PER_SECOND;
        let late_wall = wall + chrono::TimeDelta::seconds(31);
        freshness.arrive(late_at, late_wall);
        assert_eq!(
            freshness.deliverable(late_at, late_wall, 0),
            Delivery::Fresh(late_wall)
        );
    }

    #[test]
    fn an_arrival_at_the_stale_bound_is_still_deliverable() {
        let (freshness, at, wall) = arrived();
        assert!(matches!(
            freshness.deliverable(at + STALE_BOUND_100NS, wall, 0),
            Delivery::Fresh(_)
        ));
        assert!(matches!(
            freshness.deliverable(
                at,
                wall + chrono::TimeDelta::seconds(STALE_SHOT_SECONDS as i64),
                0
            ),
            Delivery::Fresh(_)
        ));
    }

    #[test]
    fn an_arrival_past_the_qpc_stale_bound_is_stale() {
        let (freshness, at, wall) = arrived();
        assert_eq!(
            freshness.deliverable(at + STALE_BOUND_100NS + 1, wall, 0),
            Delivery::Stale
        );
    }

    #[test]
    fn a_wall_clock_jump_past_the_bound_is_stale() {
        let (freshness, at, wall) = arrived();
        let jump = chrono::TimeDelta::seconds(STALE_SHOT_SECONDS as i64)
            + chrono::TimeDelta::milliseconds(1);
        assert_eq!(freshness.deliverable(at, wall + jump, 0), Delivery::Stale);
    }

    #[test]
    fn a_backward_wall_clock_does_not_refuse_delivery() {
        let (freshness, at, wall) = arrived();
        assert!(matches!(
            freshness.deliverable(at, wall - chrono::TimeDelta::minutes(10), 0),
            Delivery::Fresh(_)
        ));
    }

    #[test]
    fn a_delivered_hold_gone_old_is_idle_not_stale() {
        // Stale tears the session down; a delivered hold aging out is an idle screen and must
        // stay a quiet nothing, or every still monitor would churn its session at the bound.
        let (mut freshness, at, wall) = arrived();
        freshness.mark_delivered();
        assert_eq!(
            freshness.deliverable(
                at + STALE_BOUND_100NS + 1,
                wall + chrono::TimeDelta::seconds(31),
                0
            ),
            Delivery::Nothing
        );
    }

    #[test]
    fn an_arrival_stamped_at_the_discard_floor_is_stale() {
        // The discard promise is a boundary on the stamp: a frame composed at or before the
        // discard must not deliver, however young both clocks say it is.
        let (freshness, at, wall) = arrived();
        assert_eq!(freshness.deliverable(at, wall, COMPOSED), Delivery::Stale);
    }

    #[test]
    fn an_arrival_stamped_past_the_discard_floor_delivers() {
        let (freshness, at, wall) = arrived();
        assert!(matches!(
            freshness.deliverable(at, wall, COMPOSED - 1),
            Delivery::Fresh(_)
        ));
    }

    #[test]
    fn a_silent_fresh_session_reads_as_no_new_frame() {
        assert!(matches!(
            verdict(false, FIRST_FRAME_GRACE),
            CaptureError::Recoverable(Recoverable::NoNewFrame)
        ));
    }

    #[test]
    fn a_session_silent_past_the_grace_is_broken() {
        assert!(matches!(
            verdict(false, FIRST_FRAME_GRACE + Duration::from_millis(1)),
            CaptureError::Recoverable(Recoverable::AccessLost)
        ));
    }

    #[test]
    fn a_delivered_session_gone_idle_is_not_broken() {
        assert!(matches!(
            verdict(true, FIRST_FRAME_GRACE + Duration::from_secs(60)),
            CaptureError::Recoverable(Recoverable::NoNewFrame)
        ));
    }
}
