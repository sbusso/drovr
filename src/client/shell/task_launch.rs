//! STUB FOR INTEGRATION. Builder C owns this file (docs/design/tasks.md,
//! sections 5 and 7.4). Builder B's branch carries only the functions the
//! panel calls, with empty bodies, so the panel compiles before C lands.
//! The integrator keeps C's file and drops this one.

use super::*;
use crate::tasks::Decision;

impl ClientShellState {
    /// Opens the machine menu for a start (C).
    pub(super) fn launch_task(
        &mut self,
        _display_id: &str,
        _at: (u16, u16),
        _outcome: &mut ClientShellInput,
    ) {
    }

    /// Starts the task on one machine (C).
    pub(super) fn launch_task_on(
        &mut self,
        _display_id: &str,
        _endpoint_id: ClientEndpointId,
        _outcome: &mut ClientShellInput,
    ) {
    }

    /// The endpoint whose machine key is `machine` (C).
    pub(super) fn endpoint_for_machine(&self, machine: &str) -> Option<ClientEndpointId> {
        self.endpoints
            .iter()
            .find(|endpoint| super::projects::machine_key(endpoint) == machine)
            .map(|endpoint| endpoint.endpoint_id.clone())
    }

    /// Focuses the pane on its machine; false when it is gone or offline (C).
    pub(super) fn focus_task_pane(
        &mut self,
        _pane_key: &str,
        _outcome: &mut ClientShellInput,
    ) -> bool {
        false
    }

    /// Relays `text` to the live attempt's pane; false when not sent (C).
    pub(super) fn relay_to_task(
        &mut self,
        _display_id: &str,
        _text: &str,
        _outcome: &mut ClientShellInput,
    ) -> bool {
        false
    }

    /// Queues the ruling reply file for a remote waiting CLI (C).
    pub(super) fn publish_ruling(&mut self, _decision: &Decision, _outcome: &mut ClientShellInput) {
    }

    /// A notice in the shell's notice line (C).
    pub(super) fn push_task_notice(&mut self, message: String) -> bool {
        self.push_endpoint_notice(
            ClientEndpointNoticeKind::Rejected,
            "drovr.tasks",
            "Tasks",
            message,
        )
    }
}
