//! Vim / Evil-style modal keybinding integration for WordCraft (`Normal`, `Insert`, `Visual`).
//!
//! State is stored per `egui::Context` so upstream rebases touch only single-line hooks in
//! `canvas.rs` and `app/frame.rs` with zero changes to `WordApp` fields.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use craft_modal::defaults::default_tables;
use craft_modal::rc::load_user_rc;
use craft_modal::{
    CommandCall, ModalAction, ModalEngine, Mode, Motion, Operator, RcEval, ScrollDir, ScrollUnit, Target, TextObj, VisualKind, ZoomOp,
};
use egui::{Context, Id};
use serde_json::json;
use wordcraft_doc::Pos;

use crate::WordApp;

/// Word-processor Normal-mode bindings layered between `craft-modal` core defaults and user RC files.
const WORDCRAFT_BUILTIN_RC: &str = r#"
nnoremap a <Action>(modal.insert_after)
nnoremap I <Action>(modal.insert_line_start)
nnoremap A <Action>(modal.insert_line_end)
nnoremap o <Action>(modal.open_below)
nnoremap O <Action>(modal.open_above)
nnoremap x <Action>(modal.delete_char_fwd)
nnoremap X <Action>(modal.delete_char_back)
nnoremap s <Action>(modal.subst_char)
nnoremap S cc
nnoremap D d$
nnoremap C c$
nnoremap Y yy
nnoremap J <Action>(modal.join_lines)
"#;

/// Remembered visual selection for `gv` (`ReselectVisual`).
#[derive(Clone, Debug)]
struct SavedVisual {
    kind: VisualKind,
    anchor: Pos,
    focus: Pos,
}

/// Per-window modal state stored inside `egui::Context` memory.
pub struct WordModalState {
    pub engine: ModalEngine,
    line_anchor: HashMap<u64, (Pos, Pos)>,
    last_visual: HashMap<u64, SavedVisual>,
    reported_rc_warnings: bool,
}

impl Default for WordModalState {
    fn default() -> Self {
        let mut engine = ModalEngine::new("wordcraft");
        // Layer word-processor defaults under user RC customizations so user `.vimrc` / `.wordcraftvimrc` wins.
        engine.tables = default_tables();
        {
            let mut eval = RcEval::new("wordcraft", &mut engine.options, &mut engine.tables);
            eval.eval_str(WORDCRAFT_BUILTIN_RC, "wordcraft-defaults", None);
        }
        let _ = load_user_rc("wordcraft", &mut engine.options, &mut engine.tables);
        let start = engine.options.start_mode;
        engine.set_mode(start);
        if cfg!(test) {
            engine.options.enabled = false;
        }
        Self { engine, line_anchor: HashMap::new(), last_visual: HashMap::new(), reported_rc_warnings: false }
    }
}

fn state_handle(ctx: &Context) -> Arc<Mutex<WordModalState>> {
    let id = Id::unique("wordcraft.modal_state");
    ctx.data_mut(|d| d.get_temp_mut_or_default::<Arc<Mutex<WordModalState>>>(id).clone())
}

/// Access or mutate the window's [`WordModalState`].
pub fn with_state<R>(ctx: &Context, f: impl FnOnce(&mut WordModalState) -> R) -> Option<R> {
    let handle = state_handle(ctx);
    let mut guard = handle.lock().ok()?;
    Some(f(&mut guard))
}

/// Pre-canvas key hook called right before `crate::keys::canvas_events`.
pub(crate) fn intercept_canvas_keys(app: &mut WordApp, ctx: &Context) {
    let handle = state_handle(ctx);
    let Ok(mut state) = handle.lock() else { return };
    if !state.reported_rc_warnings {
        state.reported_rc_warnings = true;
        if let Some(first) = state.engine.warnings().first() {
            app.status_long(format!("vimrc ({}:{}): {}", first.source, first.line, first.message));
        }
    }
    if !state.engine.options.enabled {
        return;
    }

    sync_inline_escape(app, ctx, &mut state);
    let bypass = should_bypass_modal(app, ctx);
    let actions = craft_modal::egui::filter_frame(ctx, &mut state.engine, bypass);
    for action in actions {
        apply_action(app, &mut state, action);
    }
}

/// Paint the modal status badge at the bottom-right of the window.
pub(crate) fn paint_overlay(app: &WordApp, ctx: &Context) {
    if app.ui.backstage || !app.has_document() {
        return;
    }
    let handle = state_handle(ctx);
    let Ok(state) = handle.lock() else { return };
    if !state.engine.options.enabled {
        return;
    }
    craft_modal::egui::paint_mode_badge(ctx, &state.engine.status());
}

fn sync_inline_escape(app: &WordApp, ctx: &Context, state: &mut WordModalState) {
    let inline = ctx.text_edit_focused() || app.canvas.balloon.is_some() || app.session.math.is_some();
    if inline && ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { key: egui::Key::Escape, pressed: true, .. }))) {
        state.engine.set_mode(Mode::Normal);
    }
}

fn should_bypass_modal(app: &WordApp, ctx: &Context) -> bool {
    app.ui.backstage
        || app.modal_open()
        || app.ui.keytips != crate::keytips::Phase::Off
        || egui::Popup::is_any_open(ctx)
        || crate::chrome::title_search_open(app, ctx)
        || ctx.text_edit_focused()
        || app.canvas.balloon.is_some()
        || app.session.math.is_some()
        || !app.canvas.ime_preedit.is_empty()
}

fn apply_action(app: &mut WordApp, state: &mut WordModalState, action: ModalAction) {
    match action {
        ModalAction::EnterMode { mode, .. } => handle_enter_mode(app, state, mode),
        ModalAction::ReselectVisual => handle_reselect_visual(app, state),
        ModalAction::SwapVisualAnchor => handle_swap_anchor(app),
        ModalAction::Motion { motion, count, explicit_count } => handle_motion(app, state, motion, count, explicit_count),
        ModalAction::Scroll { unit, dir, count } => handle_scroll(app, state, unit, dir, count),
        ModalAction::AlignView(_) => {}
        ModalAction::Operator { op, target, .. } => handle_operator(app, state, op, target),
        ModalAction::SelectTextObject { obj, .. } => select_text_object(app, obj),
        ModalAction::Paste { before, count, .. } => handle_paste(app, !before, count),
        ModalAction::Undo { count } => repeat_cmd(app, "edit.undo", count),
        ModalAction::Redo { count } => repeat_cmd(app, "edit.redo", count),
        ModalAction::SearchStart { .. } => {
            let _ = app.run("edit.find", json!({}));
        }
        ModalAction::SearchWord { forward } => handle_search_word(app, state, forward),
        ModalAction::SearchStep { forward, count } => repeat_cmd(app, if forward { "edit.findNext" } else { "edit.findPrevious" }, count),
        ModalAction::DismissTransient => handle_dismiss(app, state),
        ModalAction::Zoom { op, count } => handle_zoom(app, op, count),
        ModalAction::JumpHistory { forward, count } => {
            if !forward {
                repeat_cmd(app, "edit.goBack", count);
            }
        }
        ModalAction::StepTab { .. } => {}
        ModalAction::StepPage { forward, count } => {
            let delta = if forward { count.max(1) as i64 } else { -(count.max(1) as i64) };
            let _ = app.run("edit.goto", json!({"pageBy": delta}));
        }
        ModalAction::ReloadConfig => reload_wordcraft_rc(app, state),
        ModalAction::AppCommand { cmd, count } => handle_app_command(app, state, &cmd, count),
    }
}

fn reload_wordcraft_rc(app: &mut WordApp, state: &mut WordModalState) {
    state.engine.tables = default_tables();
    {
        let mut eval = RcEval::new("wordcraft", &mut state.engine.options, &mut state.engine.tables);
        eval.eval_str(WORDCRAFT_BUILTIN_RC, "wordcraft-defaults", None);
    }
    let warnings = load_user_rc("wordcraft", &mut state.engine.options, &mut state.engine.tables);
    if let Some(w) = warnings.first() {
        app.status_long(format!("vimrc reloaded with warning ({}:{}): {}", w.source, w.line, w.message));
    } else {
        app.status("Reloaded modal configuration");
    }
}

fn repeat_cmd(app: &mut WordApp, id: &str, count: usize) {
    for _ in 0..count.max(1) {
        if app.run(id, json!({})).is_err() {
            break;
        }
    }
}

fn save_current_visual(app: &WordApp, state: &mut WordModalState, kind: VisualKind) {
    let doc_id = app.session.document_id();
    state.last_visual.insert(doc_id, SavedVisual { kind, anchor: app.session.sel.anchor.clone(), focus: app.session.sel.focus.clone() });
}

fn handle_enter_mode(app: &mut WordApp, state: &mut WordModalState, mode: Mode) {
    let doc_id = app.session.document_id();
    match mode {
        Mode::Normal => {
            state.line_anchor.remove(&doc_id);
            if app.session.column_mode {
                let _ = app.run("select.column", json!({"on": false}));
            }
            if !app.session.sel.is_collapsed() {
                let _ = app.run("select.collapse", json!({}));
            }
        }
        Mode::Insert => {
            state.line_anchor.remove(&doc_id);
            if app.session.column_mode {
                let _ = app.run("select.column", json!({"on": false}));
            }
            if !app.session.sel.is_collapsed() {
                let _ = app.run("select.collapse", json!({}));
            }
        }
        Mode::Visual(VisualKind::Char) => {
            state.line_anchor.remove(&doc_id);
            if app.session.column_mode {
                let _ = app.run("select.column", json!({"on": false}));
            }
        }
        Mode::Visual(VisualKind::Line) => {
            if app.session.column_mode {
                let _ = app.run("select.column", json!({"on": false}));
            }
            let _ = app.run("select.line", json!({}));
            let (a, b) = app.session.sel.ordered();
            state.line_anchor.insert(doc_id, (a, b));
        }
        Mode::Visual(VisualKind::Block) => {
            state.line_anchor.remove(&doc_id);
            let _ = app.run("select.column", json!({"on": true}));
        }
    }
}

fn handle_reselect_visual(app: &mut WordApp, state: &mut WordModalState) {
    let doc_id = app.session.document_id();
    let Some(saved) = state.last_visual.get(&doc_id).cloned() else { return };
    state.engine.set_mode(Mode::Visual(saved.kind));
    match saved.kind {
        VisualKind::Block => {
            let _ = app.run("select.column", json!({"anchor": saved.anchor, "focus": saved.focus}));
        }
        VisualKind::Line => {
            let _ = app.run("select.range", json!({"anchor": saved.anchor, "focus": saved.focus}));
            let (a, b) = app.session.sel.ordered();
            state.line_anchor.insert(doc_id, (a, b));
        }
        VisualKind::Char => {
            let _ = app.run("select.range", json!({"anchor": saved.anchor, "focus": saved.focus}));
        }
    }
}

fn handle_swap_anchor(app: &mut WordApp) {
    let anchor = app.session.sel.anchor.clone();
    let focus = app.session.sel.focus.clone();
    let _ = app.run("select.range", json!({"anchor": focus, "focus": anchor}));
}

fn handle_motion(app: &mut WordApp, state: &mut WordModalState, motion: Motion, count: usize, explicit_count: bool) {
    let ext = state.engine.mode().is_visual();
    run_motion(app, motion, count.max(1), explicit_count, ext);
    if state.engine.mode() == Mode::Visual(VisualKind::Line) {
        let doc_id = app.session.document_id();
        if let Some((start, end)) = state.line_anchor.get(&doc_id).cloned() {
            let focus = app.session.sel.focus.clone();
            let _ = app.run("select.dragUnit", json!({"unit": "line", "start": start, "end": end, "pos": focus}));
        }
    }
}

fn run_motion(app: &mut WordApp, motion: Motion, count: usize, explicit_count: bool, extend: bool) {
    let params = json!({"extend": extend});
    match motion {
        Motion::Left => repeat_with_params(app, "caret.left", &params, count),
        Motion::Right => repeat_with_params(app, "caret.right", &params, count),
        Motion::Up => repeat_with_params(app, "caret.up", &params, count),
        Motion::Down => repeat_with_params(app, "caret.down", &params, count),
        Motion::WordForward | Motion::BigWordForward | Motion::WordEnd | Motion::BigWordEnd => {
            repeat_with_params(app, "caret.wordRight", &params, count);
        }
        Motion::WordBack | Motion::BigWordBack => repeat_with_params(app, "caret.wordLeft", &params, count),
        Motion::LineStart => {
            let _ = app.run("caret.home", params);
        }
        Motion::FirstNonBlank => move_first_non_blank(app, extend),
        Motion::LineEnd => {
            let _ = app.run("caret.end", params);
        }
        Motion::ParagraphPrev => repeat_with_params(app, "caret.paraUp", &params, count),
        Motion::ParagraphNext => repeat_with_params(app, "caret.paraDown", &params, count),
        Motion::DocStart => {
            if explicit_count {
                let _ = app.run("edit.goto", json!({"page": count}));
            } else {
                let _ = app.run("caret.docStart", params);
            }
        }
        Motion::DocEnd => {
            if explicit_count {
                let _ = app.run("edit.goto", json!({"page": count}));
            } else {
                let _ = app.run("caret.docEnd", params);
            }
        }
        Motion::FindChar { ch, forward, till } => move_find_char(app, ch, forward, till, count, extend),
        Motion::ScreenTop | Motion::ScreenMiddle | Motion::ScreenBottom | Motion::RepeatFindChar { .. } => {}
    }
}

fn repeat_with_params(app: &mut WordApp, id: &str, params: &serde_json::Value, count: usize) {
    for _ in 0..count.max(1) {
        if app.run(id, params.clone()).is_err() {
            break;
        }
    }
}

fn move_first_non_blank(app: &mut WordApp, extend: bool) {
    let _ = app.run("caret.home", json!({"extend": extend}));
    let focus = app.session.sel.focus.clone();
    let Some(para) = app.session.doc.para_at(&focus) else { return };
    let tail = para.text.get(focus.off..).unwrap_or("");
    let ws_bytes: usize = tail.chars().take_while(|c| *c == ' ' || *c == '\t').map(char::len_utf8).sum();
    if ws_bytes > 0 {
        let target = Pos { off: focus.off + ws_bytes, ..focus };
        let _ = app.run("caret.set", json!({"pos": target, "extend": extend}));
    }
}

fn move_find_char(app: &mut WordApp, target_ch: char, forward: bool, till: bool, count: usize, extend: bool) {
    let focus = app.session.sel.focus.clone();
    let Some(para) = app.session.doc.para_at(&focus) else { return };
    let text = &para.text;
    let mut cur_off = focus.off.min(text.len());
    let mut found_off = None;
    for _ in 0..count.max(1) {
        if forward {
            let next_start = text.get(cur_off..).and_then(|s| s.chars().next()).map_or(cur_off, |c| cur_off + c.len_utf8());
            let Some(rel) = text.get(next_start..).and_then(|s| s.find(target_ch)) else { break };
            cur_off = next_start + rel;
            found_off = Some(cur_off);
        } else {
            let Some(rel) = text.get(..cur_off).and_then(|s| s.rfind(target_ch)) else { break };
            cur_off = rel;
            found_off = Some(cur_off);
        }
    }
    let Some(mut off) = found_off else { return };
    if till {
        if forward {
            off = text.get(..off).and_then(|s| s.char_indices().next_back()).map_or(off, |(i, _)| i);
        } else if let Some(ch) = text.get(off..).and_then(|s| s.chars().next()) {
            off = (off + ch.len_utf8()).min(text.len());
        }
    }
    let target = Pos { off, ..focus };
    let _ = app.run("caret.set", json!({"pos": target, "extend": extend}));
}

fn handle_scroll(app: &mut WordApp, state: &WordModalState, unit: ScrollUnit, dir: ScrollDir, count: usize) {
    let ext = state.engine.mode().is_visual();
    let params = json!({"extend": ext});
    let down = matches!(dir, ScrollDir::Down | ScrollDir::Right);
    match unit {
        ScrollUnit::Line => repeat_with_params(app, if down { "caret.down" } else { "caret.up" }, &params, count * 2),
        ScrollUnit::HalfPage => repeat_with_params(app, if down { "caret.down" } else { "caret.up" }, &params, count * 12),
        ScrollUnit::FullPage => repeat_with_params(app, if down { "caret.pageDown" } else { "caret.pageUp" }, &params, count),
    }
}

fn select_text_object(app: &mut WordApp, obj: TextObj) {
    let cmd = match obj {
        TextObj::Word | TextObj::BigWord => "select.word",
        TextObj::Line => "select.line",
        TextObj::Paragraph => "select.paragraph",
    };
    let _ = app.run(cmd, json!({}));
}

fn handle_operator(app: &mut WordApp, state: &mut WordModalState, op: Operator, target: Target) {
    if let Mode::Visual(kind) = state.engine.mode() {
        save_current_visual(app, state, kind);
    }
    state.line_anchor.remove(&app.session.document_id());
    prepare_operator_selection(app, target);
    execute_operator(app, state, op);
}

fn prepare_operator_selection(app: &mut WordApp, target: Target) {
    match target {
        Target::Selection => {
            if !app.session.column_mode && state_visual_was_linewise(app) {
                // Already selected by Visual mode.
            }
        }
        Target::Linewise { count } => {
            let _ = app.run("select.line", json!({}));
            for _ in 1..count.max(1) {
                let _ = app.run("caret.down", json!({"extend": true}));
                let _ = app.run("caret.end", json!({"extend": true}));
            }
        }
        Target::TextObject { obj, .. } => select_text_object(app, obj),
        Target::Motion { motion, count, explicit_count } => run_motion(app, motion, count.max(1), explicit_count, true),
    }
}

fn state_visual_was_linewise(app: &WordApp) -> bool {
    !app.session.sel.is_collapsed()
}

fn execute_operator(app: &mut WordApp, state: &mut WordModalState, op: Operator) {
    match op {
        Operator::Yank => {
            let _ = app.run("edit.copy", json!({}));
            let _ = app.run("select.collapse", json!({}));
        }
        Operator::Delete => {
            let _ = app.run("edit.cut", json!({}));
        }
        Operator::Change => {
            let _ = app.run("edit.cut", json!({}));
            state.engine.set_mode(Mode::Insert);
        }
        Operator::Highlight => {
            let _ = app.run("format.highlight", json!({}));
            let _ = app.run("select.collapse", json!({}));
        }
        Operator::Underline => {
            let _ = app.run("format.underline", json!({}));
            let _ = app.run("select.collapse", json!({}));
        }
        Operator::StrikeOut => {
            let _ = app.run("format.strikethrough", json!({}));
            let _ = app.run("select.collapse", json!({}));
        }
        Operator::Squiggly => {
            let _ = app.run("format.underline", json!({"style": "wave"}));
            let _ = app.run("select.collapse", json!({}));
        }
        Operator::Note => {
            let _ = app.run("review.newComment", json!({}));
        }
    }
}

fn handle_paste(app: &mut WordApp, after: bool, count: usize) {
    if after && app.session.sel.is_collapsed() {
        let focus = &app.session.sel.focus;
        let non_empty = app.session.doc.para_at(focus).is_some_and(|p| focus.off < p.len());
        if non_empty {
            let _ = app.run("caret.right", json!({}));
        }
    }
    repeat_cmd(app, "edit.paste", count);
}

fn handle_search_word(app: &mut WordApp, state: &WordModalState, forward: bool) {
    let _ = app.run("select.word", json!({}));
    let word = app.session.selected_text();
    let trimmed = word.trim();
    if trimmed.is_empty() {
        return;
    }
    let smart_case = state.engine.options.smart_case && trimmed.chars().any(char::is_uppercase);
    let match_case = !state.engine.options.ignore_case || smart_case;
    let cmd = if forward { "edit.findNext" } else { "edit.findPrevious" };
    let _ = app.run(cmd, json!({"text": trimmed, "matchCase": match_case}));
}

fn handle_dismiss(app: &mut WordApp, state: &mut WordModalState) {
    state.line_anchor.remove(&app.session.document_id());
    if app.session.column_mode {
        let _ = app.run("select.column", json!({"on": false}));
    }
    if !app.session.sel.is_collapsed() {
        let _ = app.run("select.collapse", json!({}));
    }
}

fn handle_zoom(app: &mut WordApp, op: ZoomOp, count: usize) {
    match op {
        ZoomOp::In => repeat_cmd(app, "view.zoomIn", count),
        ZoomOp::Out => repeat_cmd(app, "view.zoomOut", count),
        ZoomOp::Reset => {
            let _ = app.run("view.zoom100", json!({}));
        }
        ZoomOp::FitWidth => {
            let _ = app.run("view.pageWidth", json!({}));
        }
        ZoomOp::FitPage => {
            let _ = app.run("view.onePage", json!({}));
        }
    }
}

fn handle_app_command(app: &mut WordApp, state: &mut WordModalState, cmd: &CommandCall, count: usize) {
    match cmd.id.as_str() {
        "modal.insert_after" => {
            let _ = app.run("caret.right", json!({}));
            state.engine.set_mode(Mode::Insert);
        }
        "modal.insert_line_start" => {
            move_first_non_blank(app, false);
            state.engine.set_mode(Mode::Insert);
        }
        "modal.insert_line_end" => {
            let _ = app.run("caret.end", json!({}));
            state.engine.set_mode(Mode::Insert);
        }
        "modal.open_below" => {
            let _ = app.run("caret.end", json!({}));
            let _ = app.run("text.newParagraph", json!({}));
            state.engine.set_mode(Mode::Insert);
        }
        "modal.open_above" => {
            let _ = app.run("caret.home", json!({}));
            let _ = app.run("text.newParagraph", json!({}));
            let _ = app.run("caret.up", json!({}));
            state.engine.set_mode(Mode::Insert);
        }
        "modal.delete_char_fwd" => repeat_cmd(app, "text.delete", count),
        "modal.delete_char_back" => repeat_cmd(app, "text.backspace", count),
        "modal.subst_char" => {
            repeat_cmd(app, "text.delete", count);
            state.engine.set_mode(Mode::Insert);
        }
        "modal.join_lines" => {
            for _ in 0..count.max(1) {
                let _ = app.run("caret.end", json!({}));
                let _ = app.run("text.delete", json!({}));
                let _ = app.run("text.insert", json!({"text": " "}));
            }
        }
        other => {
            for _ in 0..count.max(1) {
                if app.run(other, cmd.params.clone()).is_err() {
                    break;
                }
            }
        }
    }
}
