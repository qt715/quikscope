#[path = "../settings_ui.rs"]
mod cases;
#[macro_use]
mod main_thread;

main_thread_tests![
    mouse_toggle_preserves_comment_and_applies_config,
    invalid_field_renders_error_and_leaves_file_unchanged,
    escape_discards_invalid_field_text_and_allows_close,
    invalid_existing_normal_config_shows_error_and_never_writes,
    welcome_previews_without_file_then_done_commits_live_atomically,
    welcome_done_commits_frozen_choice_too,
    welcome_refuses_config_that_became_invalid_while_open,
];
