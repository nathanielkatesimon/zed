use futures::FutureExt;
use gpui::{
    actions,
    elements::{Flex, MouseEventHandler, Padding, ParentElement, Text},
    color::Color,
    fonts::{HighlightStyle, Underline, Weight},
    platform::{CursorStyle, MouseButton},
    AnyElement, AppContext, Element, ModelHandle, MouseRegion, Task, ViewContext,
};
use language::{Bias, DiagnosticEntry, DiagnosticSeverity, Language, LanguageRegistry};
use project::{HoverBlock, HoverBlockKind, Project};
use settings::Settings;
use std::{ops::Range, sync::Arc, time::Duration};
use util::TryFutureExt;

use crate::{
    display_map::ToDisplayPoint, Anchor, AnchorRangeExt, DisplayPoint, Editor, EditorSnapshot,
    EditorStyle, GoToDiagnostic, RangeToAnchorExt,
};

pub const HOVER_DELAY_MILLIS: u64 = 350;
pub const HOVER_REQUEST_DELAY_MILLIS: u64 = 200;

pub const MIN_POPOVER_CHARACTER_WIDTH: f32 = 20.;
pub const MIN_POPOVER_LINE_HEIGHT: f32 = 4.;
pub const HOVER_POPOVER_GAP: f32 = 10.;

actions!(editor, [Hover]);

pub fn init(cx: &mut AppContext) {
    cx.add_action(hover);
}

/// Bindable action which uses the most recent selection head to trigger a hover
pub fn hover(editor: &mut Editor, _: &Hover, cx: &mut ViewContext<Editor>) {
    let head = editor.selections.newest_display(cx).head();
    show_hover(editor, head, true, cx);
}

/// The internal hover action dispatches between `show_hover` or `hide_hover`
/// depending on whether a point to hover over is provided.
pub fn hover_at(editor: &mut Editor, point: Option<DisplayPoint>, cx: &mut ViewContext<Editor>) {
    if cx.global::<Settings>().hover_popover_enabled {
        if let Some(point) = point {
            show_hover(editor, point, false, cx);
        } else {
            hide_hover(editor, cx);
        }
    }
}

/// Hides the type information popup.
/// Triggered by the `Hover` action when the cursor is not over a symbol or when the
/// selections changed.
pub fn hide_hover(editor: &mut Editor, cx: &mut ViewContext<Editor>) -> bool {
    let did_hide = editor.hover_state.info_popover.take().is_some()
        | editor.hover_state.diagnostic_popover.take().is_some();

    editor.hover_state.info_task = None;
    editor.hover_state.triggered_from = None;

    editor.clear_background_highlights::<HoverState>(cx);

    if did_hide {
        cx.notify();
    }

    did_hide
}

/// Queries the LSP and shows type info and documentation
/// about the symbol the mouse is currently hovering over.
/// Triggered by the `Hover` action when the cursor may be over a symbol.
fn show_hover(
    editor: &mut Editor,
    point: DisplayPoint,
    ignore_timeout: bool,
    cx: &mut ViewContext<Editor>,
) {
    if editor.pending_rename.is_some() {
        return;
    }

    let snapshot = editor.snapshot(cx);
    let multibuffer_offset = point.to_offset(&snapshot.display_snapshot, Bias::Left);

    let (buffer, buffer_position) = if let Some(output) = editor
        .buffer
        .read(cx)
        .text_anchor_for_position(multibuffer_offset, cx)
    {
        output
    } else {
        return;
    };

    let excerpt_id = if let Some((excerpt_id, _, _)) = editor
        .buffer()
        .read(cx)
        .excerpt_containing(multibuffer_offset, cx)
    {
        excerpt_id
    } else {
        return;
    };

    let project = if let Some(project) = editor.project.clone() {
        project
    } else {
        return;
    };

    if !ignore_timeout {
        if let Some(InfoPopover { symbol_range, .. }) = &editor.hover_state.info_popover {
            if symbol_range
                .to_offset(&snapshot.buffer_snapshot)
                .contains(&multibuffer_offset)
            {
                // Hover triggered from same location as last time. Don't show again.
                return;
            } else {
                hide_hover(editor, cx);
            }
        }
    }

    // Get input anchor
    let anchor = snapshot
        .buffer_snapshot
        .anchor_at(multibuffer_offset, Bias::Left);

    // Don't request again if the location is the same as the previous request
    if let Some(triggered_from) = &editor.hover_state.triggered_from {
        if triggered_from
            .cmp(&anchor, &snapshot.buffer_snapshot)
            .is_eq()
        {
            return;
        }
    }

    let task = cx.spawn(|this, mut cx| {
        async move {
            // If we need to delay, delay a set amount initially before making the lsp request
            let delay = if !ignore_timeout {
                // Construct delay task to wait for later
                let total_delay = Some(
                    cx.background()
                        .timer(Duration::from_millis(HOVER_DELAY_MILLIS)),
                );

                cx.background()
                    .timer(Duration::from_millis(HOVER_REQUEST_DELAY_MILLIS))
                    .await;
                total_delay
            } else {
                None
            };

            // query the LSP for hover info
            let hover_request = cx.update(|cx| {
                project.update(cx, |project, cx| {
                    project.hover(&buffer, buffer_position, cx)
                })
            });

            if let Some(delay) = delay {
                delay.await;
            }

            // If there's a diagnostic, assign it on the hover state and notify
            let local_diagnostic = snapshot
                .buffer_snapshot
                .diagnostics_in_range::<_, usize>(multibuffer_offset..multibuffer_offset, false)
                // Find the entry with the most specific range
                .min_by_key(|entry| entry.range.end - entry.range.start)
                .map(|entry| DiagnosticEntry {
                    diagnostic: entry.diagnostic,
                    range: entry.range.to_anchors(&snapshot.buffer_snapshot),
                });

            // Pull the primary diagnostic out so we can jump to it if the popover is clicked
            let primary_diagnostic = local_diagnostic.as_ref().and_then(|local_diagnostic| {
                snapshot
                    .buffer_snapshot
                    .diagnostic_group::<usize>(local_diagnostic.diagnostic.group_id)
                    .find(|diagnostic| diagnostic.diagnostic.is_primary)
                    .map(|entry| DiagnosticEntry {
                        diagnostic: entry.diagnostic,
                        range: entry.range.to_anchors(&snapshot.buffer_snapshot),
                    })
            });

            this.update(&mut cx, |this, _| {
                this.hover_state.diagnostic_popover =
                    local_diagnostic.map(|local_diagnostic| DiagnosticPopover {
                        local_diagnostic,
                        primary_diagnostic,
                    });
            })?;

            // Construct new hover popover from hover request
            let hover_popover = hover_request.await.ok().flatten().and_then(|hover_result| {
                if hover_result.contents.is_empty() {
                    return None;
                }

                // Create symbol range of anchors for highlighting and filtering
                // of future requests.
                let range = if let Some(range) = hover_result.range {
                    let start = snapshot
                        .buffer_snapshot
                        .anchor_in_excerpt(excerpt_id.clone(), range.start);
                    let end = snapshot
                        .buffer_snapshot
                        .anchor_in_excerpt(excerpt_id.clone(), range.end);

                    start..end
                } else {
                    anchor..anchor
                };

                Some(InfoPopover {
                    project: project.clone(),
                    symbol_range: range,
                    blocks: hover_result.contents,
                    rendered_content: None,
                })
            });

            this.update(&mut cx, |this, cx| {
                if let Some(hover_popover) = hover_popover.as_ref() {
                    // Highlight the selected symbol using a background highlight
                    this.highlight_background::<HoverState>(
                        vec![hover_popover.symbol_range.clone()],
                        |theme| theme.editor.hover_popover.highlight,
                        cx,
                    );
                } else {
                    this.clear_background_highlights::<HoverState>(cx);
                }

                this.hover_state.info_popover = hover_popover;
                cx.notify();
            })?;

            Ok::<_, anyhow::Error>(())
        }
        .log_err()
    });

    editor.hover_state.info_task = Some(task);
}

fn render_blocks(
    theme_id: usize,
    blocks: &[HoverBlock],
    language_registry: &Arc<LanguageRegistry>,
    style: &EditorStyle,
) -> RenderedInfo {
    let mut text = String::new();
    let mut highlights = Vec::new();
    let mut link_ranges = Vec::new();
    let mut link_urls = Vec::new();

    for block in blocks {
        match &block.kind {
            HoverBlockKind::PlainText => {
                new_paragraph(&mut text);
                text.push_str(&block.text);
            }
            HoverBlockKind::Markdown => {
                use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};

                let mut bold_depth = 0;
                let mut italic_depth = 0;
                let mut link_url = None;
                let mut current_language = None;
                let mut list_stack = Vec::new();

                for event in Parser::new_ext(&block.text, Options::all()) {
                    let prev_len = text.len();
                    match event {
                        Event::Text(t) => {
                            if let Some(language) = &current_language {
                                render_code(
                                    &mut text,
                                    &mut highlights,
                                    t.as_ref(),
                                    language,
                                    style,
                                );
                            } else {
                                text.push_str(t.as_ref());

                                let mut style = HighlightStyle::default();
                                if bold_depth > 0 {
                                    style.weight = Some(Weight::BOLD);
                                }
                                if italic_depth > 0 {
                                    style.italic = Some(true);
                                }
                                if link_url.is_some() {
                                    style.underline = Some(Underline {
                                        thickness: 1.0.into(),
                                        ..Default::default()
                                    });
                                }

                                if style != HighlightStyle::default() {
                                    let mut new_highlight = true;
                                    if let Some((last_range, last_style)) = highlights.last_mut() {
                                        if last_range.end == prev_len && last_style == &style {
                                            last_range.end = text.len();
                                            new_highlight = false;
                                        }
                                    }
                                    if new_highlight {
                                        highlights.push((prev_len..text.len(), style));
                                    }
                                }
                            }
                        }
                        Event::Code(t) => {
                            text.push_str(t.as_ref());
                            highlights.push((
                                prev_len..text.len(),
                                HighlightStyle {
                                    color: Some(Color::red()),
                                    ..Default::default()
                                },
                            ));
                        }
                        Event::Start(tag) => match tag {
                            Tag::Paragraph => new_paragraph(&mut text),
                            Tag::Heading(_, _, _) => {
                                new_paragraph(&mut text);
                                bold_depth += 1;
                            }
                            Tag::CodeBlock(kind) => {
                                new_paragraph(&mut text);
                                if let CodeBlockKind::Fenced(language) = kind {
                                    current_language = language_registry
                                        .language_for_name(language.as_ref())
                                        .now_or_never()
                                        .and_then(Result::ok);
                                }
                            }
                            Tag::Emphasis => italic_depth += 1,
                            Tag::Strong => bold_depth += 1,
                            Tag::Link(_, url, _) => link_url = Some((prev_len, url)),
                            Tag::List(number) => list_stack.push(number),
                            Tag::Item => {
                                let len = list_stack.len();
                                if let Some(list_state) = list_stack.last_mut() {
                                    new_paragraph(&mut text);
                                    for _ in 0..len - 1 {
                                        text.push_str("  ");
                                    }
                                    if let Some(number) = list_state {
                                        text.push_str(&format!("{}. ", number));
                                        *number += 1;
                                    } else {
                                        text.push_str("* ");
                                    }
                                }
                            }
                            _ => {}
                        },
                        Event::End(tag) => match tag {
                            Tag::Heading(_, _, _) => bold_depth -= 1,
                            Tag::CodeBlock(_) => current_language = None,
                            Tag::Emphasis => italic_depth -= 1,
                            Tag::Strong => bold_depth -= 1,
                            Tag::Link(_, _, _) => {
                                if let Some((start_offset, link_url)) = link_url.take() {
                                    link_ranges.push(start_offset..text.len());
                                    link_urls.push(link_url.to_string());
                                }
                            }
                            Tag::List(_) => {
                                list_stack.pop();
                            }
                            _ => {}
                        },
                        Event::HardBreak => text.push('\n'),
                        Event::SoftBreak => text.push(' '),
                        _ => {}
                    }
                }
            }
            HoverBlockKind::Code { language } => {
                if let Some(language) = language_registry
                    .language_for_name(language)
                    .now_or_never()
                    .and_then(Result::ok)
                {
                    render_code(&mut text, &mut highlights, &block.text, &language, style);
                } else {
                    text.push_str(&block.text);
                }
            }
        }
    }

    RenderedInfo {
        theme_id,
        text,
        highlights,
        link_ranges,
        link_urls,
    }
}

fn render_code(
    text: &mut String,
    highlights: &mut Vec<(Range<usize>, HighlightStyle)>,
    content: &str,
    language: &Arc<Language>,
    style: &EditorStyle,
) {
    let prev_len = text.len();
    text.push_str(content);
    for (range, highlight_id) in language.highlight_text(&content.into(), 0..content.len()) {
        if let Some(style) = highlight_id.style(&style.syntax) {
            highlights.push((prev_len + range.start..prev_len + range.end, style));
        }
    }
}

fn new_paragraph(text: &mut String) {
    if !text.is_empty() {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push('\n');
    }
}

#[derive(Default)]
pub struct HoverState {
    pub info_popover: Option<InfoPopover>,
    pub diagnostic_popover: Option<DiagnosticPopover>,
    pub triggered_from: Option<Anchor>,
    pub info_task: Option<Task<Option<()>>>,
}

impl HoverState {
    pub fn visible(&self) -> bool {
        self.info_popover.is_some() || self.diagnostic_popover.is_some()
    }

    pub fn render(
        &mut self,
        snapshot: &EditorSnapshot,
        style: &EditorStyle,
        visible_rows: Range<u32>,
        cx: &mut ViewContext<Editor>,
    ) -> Option<(DisplayPoint, Vec<AnyElement<Editor>>)> {
        // If there is a diagnostic, position the popovers based on that.
        // Otherwise use the start of the hover range
        let anchor = self
            .diagnostic_popover
            .as_ref()
            .map(|diagnostic_popover| &diagnostic_popover.local_diagnostic.range.start)
            .or_else(|| {
                self.info_popover
                    .as_ref()
                    .map(|info_popover| &info_popover.symbol_range.start)
            })?;
        let point = anchor.to_display_point(&snapshot.display_snapshot);

        // Don't render if the relevant point isn't on screen
        if !self.visible() || !visible_rows.contains(&point.row()) {
            return None;
        }

        let mut elements = Vec::new();

        if let Some(diagnostic_popover) = self.diagnostic_popover.as_ref() {
            elements.push(diagnostic_popover.render(style, cx));
        }
        if let Some(info_popover) = self.info_popover.as_mut() {
            elements.push(info_popover.render(style, cx));
        }

        Some((point, elements))
    }
}

#[derive(Debug, Clone)]
pub struct InfoPopover {
    pub project: ModelHandle<Project>,
    pub symbol_range: Range<Anchor>,
    pub blocks: Vec<HoverBlock>,
    rendered_content: Option<RenderedInfo>,
}

#[derive(Debug, Clone)]
struct RenderedInfo {
    theme_id: usize,
    text: String,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    link_ranges: Vec<Range<usize>>,
    link_urls: Vec<String>,
}

impl InfoPopover {
    pub fn render(
        &mut self,
        style: &EditorStyle,
        cx: &mut ViewContext<Editor>,
    ) -> AnyElement<Editor> {
        if let Some(rendered) = &self.rendered_content {
            if rendered.theme_id != style.theme_id {
                self.rendered_content = None;
            }
        }

        let rendered_content = self.rendered_content.get_or_insert_with(|| {
            render_blocks(
                style.theme_id,
                &self.blocks,
                self.project.read(cx).languages(),
                style,
            )
        });

        MouseEventHandler::<InfoPopover, _>::new(0, cx, |_, cx| {
            let mut region_id = 0;
            let view_id = cx.view_id();

            let link_urls = rendered_content.link_urls.clone();
            Flex::column()
                .scrollable::<HoverBlock>(1, None, cx)
                .with_child(
                    Text::new(rendered_content.text.clone(), style.text.clone())
                        .with_highlights(rendered_content.highlights.clone())
                        .with_mouse_regions(
                            rendered_content.link_ranges.clone(),
                            move |ix, bounds| {
                                region_id += 1;
                                let url = link_urls[ix].clone();
                                MouseRegion::new::<Self>(view_id, region_id, bounds)
                                    .on_click::<Editor, _>(MouseButton::Left, move |_, _, cx| {
                                        println!("clicked link {url}");
                                        cx.platform().open_url(&url);
                                    })
                            },
                        )
                        .with_soft_wrap(true),
                )
                .contained()
                .with_style(style.hover_popover.container)
        })
        .on_move(|_, _, _| {}) // Consume move events so they don't reach regions underneath.
        .with_cursor_style(CursorStyle::Arrow)
        .with_padding(Padding {
            bottom: HOVER_POPOVER_GAP,
            top: HOVER_POPOVER_GAP,
            ..Default::default()
        })
        .into_any()
    }
}

#[derive(Debug, Clone)]
pub struct DiagnosticPopover {
    local_diagnostic: DiagnosticEntry<Anchor>,
    primary_diagnostic: Option<DiagnosticEntry<Anchor>>,
}

impl DiagnosticPopover {
    pub fn render(&self, style: &EditorStyle, cx: &mut ViewContext<Editor>) -> AnyElement<Editor> {
        enum PrimaryDiagnostic {}

        let mut text_style = style.hover_popover.prose.clone();
        text_style.font_size = style.text.font_size;
        let diagnostic_source_style = style.hover_popover.diagnostic_source_highlight.clone();

        let text = match &self.local_diagnostic.diagnostic.source {
            Some(source) => Text::new(
                format!("{source}: {}", self.local_diagnostic.diagnostic.message),
                text_style,
            )
            .with_highlights(vec![(0..source.len(), diagnostic_source_style)]),

            None => Text::new(self.local_diagnostic.diagnostic.message.clone(), text_style),
        };

        let container_style = match self.local_diagnostic.diagnostic.severity {
            DiagnosticSeverity::HINT => style.hover_popover.info_container,
            DiagnosticSeverity::INFORMATION => style.hover_popover.info_container,
            DiagnosticSeverity::WARNING => style.hover_popover.warning_container,
            DiagnosticSeverity::ERROR => style.hover_popover.error_container,
            _ => style.hover_popover.container,
        };

        let tooltip_style = cx.global::<Settings>().theme.tooltip.clone();

        MouseEventHandler::<DiagnosticPopover, _>::new(0, cx, |_, _| {
            text.with_soft_wrap(true)
                .contained()
                .with_style(container_style)
        })
        .with_padding(Padding {
            top: HOVER_POPOVER_GAP,
            bottom: HOVER_POPOVER_GAP,
            ..Default::default()
        })
        .on_move(|_, _, _| {}) // Consume move events so they don't reach regions underneath.
        .on_click(MouseButton::Left, |_, _, cx| {
            cx.dispatch_action(GoToDiagnostic)
        })
        .with_cursor_style(CursorStyle::PointingHand)
        .with_tooltip::<PrimaryDiagnostic>(
            0,
            "Go To Diagnostic".to_string(),
            Some(Box::new(crate::GoToDiagnostic)),
            tooltip_style,
            cx,
        )
        .into_any()
    }

    pub fn activation_info(&self) -> (usize, Anchor) {
        let entry = self
            .primary_diagnostic
            .as_ref()
            .unwrap_or(&self.local_diagnostic);

        (entry.diagnostic.group_id, entry.range.start.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::editor_lsp_test_context::EditorLspTestContext;
    use gpui::fonts::Weight;
    use indoc::indoc;
    use language::{Diagnostic, DiagnosticSet};
    use lsp::LanguageServerId;
    use project::{HoverBlock, HoverBlockKind};
    use smol::stream::StreamExt;
    use util::test::marked_text_ranges;

    #[gpui::test]
    async fn test_mouse_hover_info_popover(cx: &mut gpui::TestAppContext) {
        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
                ..Default::default()
            },
            cx,
        )
        .await;

        // Basic hover delays and then pops without moving the mouse
        cx.set_state(indoc! {"
            fn ˇtest() { println!(); }
        "});
        let hover_point = cx.display_point(indoc! {"
            fn test() { printˇln!(); }
        "});

        cx.update_editor(|editor, cx| hover_at(editor, Some(hover_point), cx));
        assert!(!cx.editor(|editor, _| editor.hover_state.visible()));

        // After delay, hover should be visible.
        let symbol_range = cx.lsp_range(indoc! {"
            fn test() { «println!»(); }
        "});
        let mut requests =
            cx.handle_request::<lsp::request::HoverRequest, _, _>(move |_, _, _| async move {
                Ok(Some(lsp::Hover {
                    contents: lsp::HoverContents::Markup(lsp::MarkupContent {
                        kind: lsp::MarkupKind::Markdown,
                        value: "some basic docs".to_string(),
                    }),
                    range: Some(symbol_range),
                }))
            });
        cx.foreground()
            .advance_clock(Duration::from_millis(HOVER_DELAY_MILLIS + 100));
        requests.next().await;

        cx.editor(|editor, _| {
            assert!(editor.hover_state.visible());
            assert_eq!(
                editor.hover_state.info_popover.clone().unwrap().blocks,
                vec![HoverBlock {
                    text: "some basic docs".to_string(),
                    kind: HoverBlockKind::Markdown,
                },]
            )
        });

        // Mouse moved with no hover response dismisses
        let hover_point = cx.display_point(indoc! {"
            fn teˇst() { println!(); }
        "});
        let mut request = cx
            .lsp
            .handle_request::<lsp::request::HoverRequest, _, _>(|_, _| async move { Ok(None) });
        cx.update_editor(|editor, cx| hover_at(editor, Some(hover_point), cx));
        cx.foreground()
            .advance_clock(Duration::from_millis(HOVER_DELAY_MILLIS + 100));
        request.next().await;
        cx.editor(|editor, _| {
            assert!(!editor.hover_state.visible());
        });
    }

    #[gpui::test]
    async fn test_keyboard_hover_info_popover(cx: &mut gpui::TestAppContext) {
        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
                ..Default::default()
            },
            cx,
        )
        .await;

        // Hover with keyboard has no delay
        cx.set_state(indoc! {"
            fˇn test() { println!(); }
        "});
        cx.update_editor(|editor, cx| hover(editor, &Hover, cx));
        let symbol_range = cx.lsp_range(indoc! {"
            «fn» test() { println!(); }
        "});
        cx.handle_request::<lsp::request::HoverRequest, _, _>(move |_, _, _| async move {
            Ok(Some(lsp::Hover {
                contents: lsp::HoverContents::Markup(lsp::MarkupContent {
                    kind: lsp::MarkupKind::Markdown,
                    value: "some other basic docs".to_string(),
                }),
                range: Some(symbol_range),
            }))
        })
        .next()
        .await;

        cx.condition(|editor, _| editor.hover_state.visible()).await;
        cx.editor(|editor, _| {
            assert_eq!(
                editor.hover_state.info_popover.clone().unwrap().blocks,
                vec![HoverBlock {
                    text: "some other basic docs".to_string(),
                    kind: HoverBlockKind::Markdown,
                }]
            )
        });
    }

    #[gpui::test]
    async fn test_hover_diagnostic_and_info_popovers(cx: &mut gpui::TestAppContext) {
        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
                ..Default::default()
            },
            cx,
        )
        .await;

        // Hover with just diagnostic, pops DiagnosticPopover immediately and then
        // info popover once request completes
        cx.set_state(indoc! {"
            fn teˇst() { println!(); }
        "});

        // Send diagnostic to client
        let range = cx.text_anchor_range(indoc! {"
            fn «test»() { println!(); }
        "});
        cx.update_buffer(|buffer, cx| {
            let snapshot = buffer.text_snapshot();
            let set = DiagnosticSet::from_sorted_entries(
                vec![DiagnosticEntry {
                    range,
                    diagnostic: Diagnostic {
                        message: "A test diagnostic message.".to_string(),
                        ..Default::default()
                    },
                }],
                &snapshot,
            );
            buffer.update_diagnostics(LanguageServerId(0), set, cx);
        });

        // Hover pops diagnostic immediately
        cx.update_editor(|editor, cx| hover(editor, &Hover, cx));
        cx.foreground().run_until_parked();

        cx.editor(|Editor { hover_state, .. }, _| {
            assert!(hover_state.diagnostic_popover.is_some() && hover_state.info_popover.is_none())
        });

        // Info Popover shows after request responded to
        let range = cx.lsp_range(indoc! {"
            fn «test»() { println!(); }
        "});
        cx.handle_request::<lsp::request::HoverRequest, _, _>(move |_, _, _| async move {
            Ok(Some(lsp::Hover {
                contents: lsp::HoverContents::Markup(lsp::MarkupContent {
                    kind: lsp::MarkupKind::Markdown,
                    value: "some new docs".to_string(),
                }),
                range: Some(range),
            }))
        });
        cx.foreground()
            .advance_clock(Duration::from_millis(HOVER_DELAY_MILLIS + 100));

        cx.foreground().run_until_parked();
        cx.editor(|Editor { hover_state, .. }, _| {
            hover_state.diagnostic_popover.is_some() && hover_state.info_task.is_some()
        });
    }

    #[gpui::test]
    fn test_render_blocks(cx: &mut gpui::TestAppContext) {
        Settings::test_async(cx);
        cx.add_window(|cx| {
            let editor = Editor::single_line(None, cx);
            let style = editor.style(cx);

            struct Row {
                blocks: Vec<HoverBlock>,
                expected_marked_text: &'static str,
                expected_styles: Vec<HighlightStyle>,
            }

            let rows = &[
                Row {
                    blocks: vec![HoverBlock {
                        text: "one **two** three".to_string(),
                        kind: HoverBlockKind::Markdown,
                    }],
                    expected_marked_text: "one «two» three",
                    expected_styles: vec![HighlightStyle {
                        weight: Some(Weight::BOLD),
                        ..Default::default()
                    }],
                },
                Row {
                    blocks: vec![HoverBlock {
                        text: "one [two](the-url) three".to_string(),
                        kind: HoverBlockKind::Markdown,
                    }],
                    expected_marked_text: "one «two» three",
                    expected_styles: vec![HighlightStyle {
                        underline: Some(Underline {
                            thickness: 1.0.into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                },
            ];

            for Row {
                blocks,
                expected_marked_text,
                expected_styles,
            } in rows
            {
                let rendered = render_blocks(0, &blocks, &Default::default(), &style);

                let (expected_text, ranges) = marked_text_ranges(expected_marked_text, false);
                let expected_highlights = ranges
                    .into_iter()
                    .zip(expected_styles.iter().cloned())
                    .collect::<Vec<_>>();
                assert_eq!(
                    rendered.text, expected_text,
                    "wrong text for input {blocks:?}"
                );
                assert_eq!(
                    rendered.highlights, expected_highlights,
                    "wrong highlights for input {blocks:?}"
                );
            }

            editor
        });
    }
}
