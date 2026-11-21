#[path = "../video_editor_ui.rs"]
mod cases;
#[macro_use]
mod main_thread;

main_thread_tests![
    trim_handles_and_keys_keep_the_playhead_inside_the_trim,
    format_menu_selects_formats_and_closes_on_escape_or_outside,
    mute_needs_audio_and_toggles_with_m,
    shortcut_help_opens_and_closes_without_side_effects,
    real_video_seeks_steps_and_plays,
    real_video_exports_trimmed_gif_and_mp4,
    export_can_be_cancelled_and_leaves_no_file,
    untouched_save_copies_the_source_exactly,
    save_as_writes_the_chosen_file_and_ctrl_s_updates_it,
    closing_the_window_lets_the_export_finish,
    annotate_frame_opens_a_full_size_png_from_the_clip_cache,
    audible_playback_opens_output_and_mute_closes_it,
];
