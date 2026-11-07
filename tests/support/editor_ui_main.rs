#[path = "../editor_ui.rs"]
mod cases;
#[macro_use]
mod main_thread;

main_thread_tests![
    input_method_composition_commits_into_the_text,
    formatting_shortcuts_apply_while_typing_and_to_selected_text,
    color_picker_drag_recolors_the_selection_as_one_undo_step,
    field_key_edits_end_ime_composition,
    zoom_moves_in_tenths_below_actual_size_and_quarters_above,
    drawing_past_the_edge_refits_the_view_to_the_contents,
    select_tool_drags_text_and_hollow_shapes_from_inside_once_selected,
    marquee_contains_bounds_and_shift_click_toggles,
    multi_move_and_nudge_each_record_one_undo_step,
    multi_style_and_delete_are_atomic,
    duplicate_selection_has_offset_new_ids_and_one_undo_step,
    selection_stacking_preserves_relative_order_and_undo,
    shortcut_help_toggles_and_escape_keeps_editor_open,
    shortcut_help_fits_small_windows_and_scrolls,
];
