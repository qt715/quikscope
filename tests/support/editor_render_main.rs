#[path = "../editor_render.rs"]
mod cases;
#[macro_use]
mod main_thread;

main_thread_tests![
    editor_canvas_matches_the_exported_png,
    large_images_are_scaled_to_fit_the_window,
];
