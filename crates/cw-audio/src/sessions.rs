use std::collections::HashSet;

use wasapi::{AudioSessionManager, DeviceEnumerator, Direction, SessionState, WasapiError};

const REFRESH_EVERY_PATROLS: u32 = 10;

#[derive(Default)]
pub(crate) struct Sessions {
    managers: Vec<AudioSessionManager>,
    patrols_left: u32,
}

impl Sessions {
    pub(crate) fn active_pids(&mut self) -> Result<HashSet<u32>, WasapiError> {
        if self.patrols_left == 0 {
            self.managers = managers()?;
            self.patrols_left = REFRESH_EVERY_PATROLS;
        }
        self.patrols_left -= 1;

        let mut active = HashSet::new();
        for manager in &self.managers {
            if let Err(error) = active_on(manager, &mut active) {
                tracing::debug!(%error, "an output device's sessions could not be listed");
                self.patrols_left = 0;
            }
        }
        Ok(active)
    }
}

fn managers() -> Result<Vec<AudioSessionManager>, WasapiError> {
    let devices = DeviceEnumerator::new()?.get_device_collection(&Direction::Render)?;
    Ok((0..devices.get_nbr_devices()?)
        .filter_map(|index| {
            devices
                .get_device_at_index(index)
                .and_then(|device| device.get_iaudiosessionmanager())
                .inspect_err(
                    |error| tracing::debug!(%error, "an output device could not be opened"),
                )
                .ok()
        })
        .collect())
}

fn active_on(manager: &AudioSessionManager, active: &mut HashSet<u32>) -> Result<(), WasapiError> {
    let sessions = manager.get_audiosessionenumerator()?;
    for index in 0..sessions.get_count()? {
        let Ok(session) = sessions.get_session(index) else {
            continue;
        };
        if matches!(session.get_state(), Ok(SessionState::Active))
            && let Ok(pid) = session.get_process_id()
        {
            active.insert(pid);
        }
    }
    Ok(())
}
