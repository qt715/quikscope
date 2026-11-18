#[path = "../video_editor_ui.rs"]
mod cases;
#[macro_use]
mod main_thread;

main_thread_tests![
    trim_handles_and_keys_keep_the_playhead_inside_the_trim,
    format_menu_selects_formats_and_closes_on_escape_or_outside,
    mute_needs_audio_and_toggles_with_m,
    real_video_seeks_steps_and_plays,
    real_video_exports_trimmed_gif_and_mp4,
    export_can_be_cancelled_and_leaves_no_file,
    annotate_frame_saves_full_size_png_and_dispatches_action,
    audible_playback_opens_output_and_mute_closes_it,
];
