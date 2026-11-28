//! Native Infimum Code Agent Loop, Shared Conversations (`~/.config/infimum-code/conversations`),
//! and Antigravity Brain Session Importer (`~/.gemini/antigravity/brain`).
//!
//! 100% compatible with the browser Infimum Code conversation JSON schema so users can
//! seamlessly switch between browser Infimum Code and Infimum Code Native (.ifw).

mod conversations;
mod dev_tools;
mod providers;
mod repo_index;
mod timeline;
mod tool_specs;
mod tools;

#[cfg(test)]
mod tests;

#[allow(unused_imports)]
pub use conversations::{
    active_conv_cell, active_conv_header, add_pending_image, clean_antigravity_user_text,
    clear_pending_images, has_pending_images, import_antigravity_session_dir,
    list_antigravity_sessions, list_saved_conversations, load_conversation_from_path,
    load_native_settings, pending_images_meta, remove_pending_image, save_active_conversation,
    save_native_settings, set_loaded_conv_gpu_images, start_new_conversation,
    take_loaded_conv_gpu_images, take_pending_images, ActiveConversation, AttachedImage,
    ConvMessage, NativeSettings,
};
#[allow(unused_imports)]
pub use providers::{run_native_agent_loop, AgentResult};
#[allow(unused_imports)]
pub use timeline::{
    encode_timeline_words, get_live_timeline_words, kill_active_shell_command, reset_live_timeline,
    signal_pid, signal_process_group, AgentStep, TimelineSnapshot, ASYNC_CANCELLED, ASYNC_CMD_PID,
    ASYNC_CMD_KILL_REQUESTED, ASYNC_GEMINI_PID, SIGKILL_NUM, SIGTERM_NUM,
};
#[allow(unused_imports)]
pub use tools::{
    add_allowed_workspace, delete_conversations_for_project, is_sensitive_path,
    load_allowed_workspaces, remove_allowed_workspace, resolve_whitelisted_project,
    save_allowed_workspaces,
};

pub(crate) use tools::{resolve_infimum_bin, resolve_infimum_web_bin};

#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use providers::test_cmd_lock;
