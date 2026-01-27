use std::sync::Arc;
use sysinfo::{
    MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System
};
use shared::model::SystemInfo;
use crate::api::model::{AppState};


pub fn exec_system_usage(app_state: &Arc<AppState>) -> tokio::task::JoinHandle<()> {
    let state = Arc::clone(app_state);

    tokio::spawn(async move {
        let pid = Pid::from_u32(std::process::id());

        let refresh_kind = RefreshKind::nothing()
            .with_memory(MemoryRefreshKind::nothing().with_ram())
            .with_processes(
                ProcessRefreshKind::nothing()
                    .with_cpu()
                    .with_memory()
            );

        let mut sys = System::new_with_specifics(refresh_kind);
        loop {
            sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
            sys.refresh_memory();

            if let Some(proc) = sys.processes().get(&pid) {
                let info = SystemInfo {
                    cpu_usage: proc.cpu_usage(),
                    memory_usage: proc.memory(),
                    memory_total: sys.total_memory(),
                };

                state.event_manager.send_system_info(info);
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    })
}