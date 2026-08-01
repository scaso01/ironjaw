pub mod canvas;
pub mod event_publish;
pub mod grep;
pub mod image_gen;
pub mod list_files;
pub mod location;
pub mod patch;
pub mod powershell;
pub mod process;
pub mod read_file;
pub mod restore_file;
pub mod schedule_create;
pub mod schedule_list;
pub mod schedule_toggle;
pub mod self_knowledge;
pub mod shell;
pub mod todo;
pub mod verify;
pub mod whoami;
pub mod write_file;

use crate::snapshots::FileSnapshotStore;
use crate::ToolRegistry;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Register all built-in tools into the registry.
pub fn register_builtins(registry: &mut ToolRegistry) {
    registry.register(Box::new(grep::GrepTool::new()));
    registry.register(Box::new(shell::ShellTool::new()));
    registry.register(Box::new(powershell::PowerShellTool::new()));
    registry.register(Box::new(read_file::ReadFileTool::new()));
    registry.register(Box::new(write_file::WriteFileTool::new()));
    registry.register(Box::new(list_files::ListFilesTool::new()));
    registry.register(Box::new(schedule_list::ScheduleListTool::new()));
    registry.register(Box::new(schedule_create::ScheduleCreateTool::new()));
    registry.register(Box::new(schedule_toggle::ScheduleToggleTool::new()));
    registry.register(Box::new(event_publish::EventPublishTool::new()));
    registry.register(Box::new(patch::ApplyPatchTool::new()));
    registry.register(Box::new(image_gen::ImageGenTool::new()));
    registry.register(Box::new(canvas::CanvasTool::new()));
    registry.register(Box::new(location::LocationTool::new()));
    registry.register(Box::new(self_knowledge::SelfKnowledgeTool::new()));
    registry.register(Box::new(verify::VerifyTool::new()));
    registry.register(Box::new(whoami::WhoamiTool::new()));

    // Process management tools share a single ProcessManager
    let process_mgr = process::ProcessManager::new();
    registry.register(Box::new(process::ProcessStartTool::new(
        process_mgr.clone(),
    )));
    registry.register(Box::new(process::ProcessPollTool::new(process_mgr.clone())));
    registry.register(Box::new(process::ProcessWriteTool::new(
        process_mgr.clone(),
    )));
    registry.register(Box::new(process::ProcessKillTool::new(process_mgr.clone())));
    registry.register(Box::new(process::ProcessListTool::new(process_mgr)));
}

/// Register the restore_file tool with a shared snapshot store.
pub fn register_restore_file(
    registry: &mut ToolRegistry,
    snapshots: Arc<Mutex<FileSnapshotStore>>,
) {
    registry.register(Box::new(restore_file::RestoreFileTool::new(snapshots)));
}

/// Register the todo tool. The tool accesses brain at runtime via ToolContext.
pub fn register_todo(registry: &mut ToolRegistry) {
    registry.register(Box::new(todo::TodoTool::new()));
}
