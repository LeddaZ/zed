use futures::future::join_all;
use itertools::Itertools;
use language::language_settings::language_settings;
use text::BufferId;
use ui::{Context, Window};

use crate::{Editor, LSP_REQUEST_DEBOUNCE_TIMEOUT};

impl Editor {
    pub(super) fn refresh_folding_ranges(
        &mut self,
        for_buffer: Option<BufferId>,
        _window: &Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode().is_full() {
            return;
        }
        let Some(project) = self.project.clone() else {
            return;
        };
        if !self.use_lsp_folding_ranges {
            return;
        }

        let buffers_to_query = self
            .visible_excerpts(true, cx)
            .into_values()
            .map(|(buffer, ..)| buffer)
            .chain(for_buffer.and_then(|id| self.buffer.read(cx).buffer(id)))
            .filter(|buffer| {
                let id = buffer.read(cx).remote_id();
                (for_buffer.is_none_or(|target| target == id))
                    && self.registered_buffers.contains_key(&id)
                    && language_settings(
                        buffer.read(cx).language().map(|l| l.name()),
                        buffer.read(cx).file(),
                        cx,
                    )
                    .lsp_folding_ranges
                    .enabled()
            })
            .unique_by(|buffer| buffer.read(cx).remote_id())
            .collect::<Vec<_>>();

        self.refresh_folding_ranges_task = cx.spawn(async move |editor, cx| {
            cx.background_executor()
                .timer(LSP_REQUEST_DEBOUNCE_TIMEOUT)
                .await;

            let Some(tasks) = editor
                .update(cx, |_, cx| {
                    project.read(cx).lsp_store().update(cx, |lsp_store, cx| {
                        buffers_to_query
                            .into_iter()
                            .map(|buffer| {
                                let buffer_id = buffer.read(cx).remote_id();
                                let task = lsp_store.fetch_folding_ranges(&buffer, cx);
                                async move { (buffer_id, task.await) }
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .ok()
            else {
                return;
            };

            let results = join_all(tasks).await;
            if results.is_empty() {
                return;
            }

            editor
                .update(cx, |editor, cx| {
                    editor.display_map.update(cx, |display_map, cx| {
                        for (buffer_id, ranges) in results {
                            display_map.set_lsp_folding_ranges(buffer_id, ranges, cx);
                        }
                    });
                    cx.notify();
                })
                .ok();
        });
    }

    pub fn lsp_folding_ranges_enabled(&self, cx: &ui::App) -> bool {
        self.use_lsp_folding_ranges && self.display_map.read(cx).has_lsp_folding_ranges()
    }

    /// Removes LSP folding creases for buffers whose `lsp_folding_ranges`
    /// setting has been turned off, and triggers a refresh so newly-enabled
    /// buffers get their ranges fetched.
    pub(super) fn clear_disabled_lsp_folding_ranges(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.use_lsp_folding_ranges {
            return;
        }

        let buffers_to_clear: Vec<BufferId> = self
            .buffer
            .read(cx)
            .all_buffers()
            .into_iter()
            .filter(|buffer| {
                let buffer = buffer.read(cx);
                !language_settings(buffer.language().map(|l| l.name()), buffer.file(), cx)
                    .lsp_folding_ranges
                    .enabled()
            })
            .map(|buffer| buffer.read(cx).remote_id())
            .collect();

        if !buffers_to_clear.is_empty() {
            self.display_map.update(cx, |display_map, cx| {
                for buffer_id in buffers_to_clear {
                    display_map.clear_lsp_folding_ranges(buffer_id, cx);
                }
            });
            cx.notify();
        }

        self.refresh_folding_ranges(None, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;
    use gpui::TestAppContext;
    use lsp::FoldingRange;
    use multi_buffer::MultiBufferRow;
    use settings::LspFoldingRanges;

    use crate::{
        editor_tests::{init_test, update_test_language_settings},
        test::editor_lsp_test_context::EditorLspTestContext,
    };

    #[gpui::test]
    async fn test_lsp_folding_ranges_populates_creases(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        update_test_language_settings(cx, |settings| {
            settings.defaults.lsp_folding_ranges = Some(LspFoldingRanges::On);
        });

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                folding_range_provider: Some(lsp::FoldingRangeProviderCapability::Simple(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let mut folding_request = cx
            .set_request_handler::<lsp::request::FoldingRangeRequest, _, _>(
                move |_, _, _| async move {
                    Ok(Some(vec![
                        FoldingRange {
                            start_line: 0,
                            start_character: Some(10),
                            end_line: 4,
                            end_character: Some(1),
                            kind: None,
                            collapsed_text: None,
                        },
                        FoldingRange {
                            start_line: 1,
                            start_character: Some(13),
                            end_line: 3,
                            end_character: Some(5),
                            kind: None,
                            collapsed_text: None,
                        },
                        FoldingRange {
                            start_line: 6,
                            start_character: Some(11),
                            end_line: 8,
                            end_character: Some(1),
                            kind: None,
                            collapsed_text: None,
                        },
                    ]))
                },
            );

        cx.set_state(
            "ˇfn main() {\n    if true {\n        println!(\"hello\");\n    }\n}\n\nfn other() {\n    let x = 1;\n}\n",
        );
        assert!(folding_request.next().await.is_some());
        cx.run_until_parked();

        cx.editor.read_with(&cx.cx.cx, |editor, cx| {
            assert!(
                editor.lsp_folding_ranges_enabled(cx),
                "Expected LSP folding ranges to be populated"
            );
        });

        cx.update_editor(|editor, _window, cx| {
            let snapshot = editor.display_snapshot(cx);
            assert!(
                !snapshot.is_line_folded(MultiBufferRow(0)),
                "Line 0 should not be folded before any fold action"
            );
            assert!(
                !snapshot.is_line_folded(MultiBufferRow(6)),
                "Line 6 should not be folded before any fold action"
            );
        });

        cx.update_editor(|editor, window, cx| {
            editor.fold_at(MultiBufferRow(0), window, cx);
        });

        cx.update_editor(|editor, _window, cx| {
            let snapshot = editor.display_snapshot(cx);
            assert!(
                snapshot.is_line_folded(MultiBufferRow(0)),
                "Line 0 should be folded after fold_at on an LSP crease"
            );
            assert_eq!(
                editor.display_text(cx),
                "fn main() ⋯\n\nfn other() {\n    let x = 1;\n}\n",
            );
        });

        cx.update_editor(|editor, window, cx| {
            editor.fold_at(MultiBufferRow(6), window, cx);
        });

        cx.update_editor(|editor, _window, cx| {
            let snapshot = editor.display_snapshot(cx);
            assert!(
                snapshot.is_line_folded(MultiBufferRow(6)),
                "Line 6 should be folded after fold_at on the second LSP crease"
            );
            assert_eq!(editor.display_text(cx), "fn main() ⋯\n\nfn other() ⋯\n",);
        });
    }

    #[gpui::test]
    async fn test_lsp_folding_ranges_disabled_by_default(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                folding_range_provider: Some(lsp::FoldingRangeProviderCapability::Simple(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        cx.set_state("ˇfn main() {\n    let x = 1;\n}\n");
        cx.run_until_parked();

        cx.editor.read_with(&cx.cx.cx, |editor, cx| {
            assert!(
                !editor.lsp_folding_ranges_enabled(cx),
                "LSP folding ranges should not be enabled by default"
            );
        });
    }

    #[gpui::test]
    async fn test_lsp_folding_ranges_toggling_off_removes_creases(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        update_test_language_settings(cx, |settings| {
            settings.defaults.lsp_folding_ranges = Some(LspFoldingRanges::On);
        });

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                folding_range_provider: Some(lsp::FoldingRangeProviderCapability::Simple(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let mut folding_request = cx
            .set_request_handler::<lsp::request::FoldingRangeRequest, _, _>(
                move |_, _, _| async move {
                    Ok(Some(vec![FoldingRange {
                        start_line: 0,
                        start_character: Some(10),
                        end_line: 4,
                        end_character: Some(1),
                        kind: None,
                        collapsed_text: None,
                    }]))
                },
            );

        cx.set_state("ˇfn main() {\n    if true {\n        println!(\"hello\");\n    }\n}\n");
        assert!(folding_request.next().await.is_some());
        cx.run_until_parked();

        cx.editor.read_with(&cx.cx.cx, |editor, cx| {
            assert!(
                editor.lsp_folding_ranges_enabled(cx),
                "Expected LSP folding ranges to be active before toggling off"
            );
        });

        cx.update_editor(|editor, window, cx| {
            editor.fold_at(MultiBufferRow(0), window, cx);
        });
        cx.update_editor(|editor, _window, cx| {
            let snapshot = editor.display_snapshot(cx);
            assert!(
                snapshot.is_line_folded(MultiBufferRow(0)),
                "Line 0 should be folded via LSP crease before toggling off"
            );
            assert_eq!(editor.display_text(cx), "fn main() ⋯\n",);
        });

        update_test_language_settings(&mut cx.cx.cx, |settings| {
            settings.defaults.lsp_folding_ranges = Some(LspFoldingRanges::Off);
        });
        cx.run_until_parked();

        cx.editor.read_with(&cx.cx.cx, |editor, cx| {
            assert!(
                !editor.lsp_folding_ranges_enabled(cx),
                "LSP folding ranges should be cleared after toggling off"
            );
        });
    }

    #[gpui::test]
    async fn test_lsp_folding_ranges_nested_folds(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        update_test_language_settings(cx, |settings| {
            settings.defaults.lsp_folding_ranges = Some(LspFoldingRanges::On);
        });

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                folding_range_provider: Some(lsp::FoldingRangeProviderCapability::Simple(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let mut folding_request = cx
            .set_request_handler::<lsp::request::FoldingRangeRequest, _, _>(
                move |_, _, _| async move {
                    Ok(Some(vec![
                        FoldingRange {
                            start_line: 0,
                            start_character: Some(10),
                            end_line: 7,
                            end_character: Some(1),
                            kind: None,
                            collapsed_text: None,
                        },
                        FoldingRange {
                            start_line: 1,
                            start_character: Some(12),
                            end_line: 3,
                            end_character: Some(5),
                            kind: None,
                            collapsed_text: None,
                        },
                        FoldingRange {
                            start_line: 4,
                            start_character: Some(13),
                            end_line: 6,
                            end_character: Some(5),
                            kind: None,
                            collapsed_text: None,
                        },
                    ]))
                },
            );

        cx.set_state(
            "ˇfn main() {\n    if true {\n        a();\n    }\n    if false {\n        b();\n    }\n}\n",
        );
        assert!(folding_request.next().await.is_some());
        cx.run_until_parked();

        cx.update_editor(|editor, window, cx| {
            editor.fold_at(MultiBufferRow(1), window, cx);
        });
        cx.update_editor(|editor, _window, cx| {
            let snapshot = editor.display_snapshot(cx);
            assert!(snapshot.is_line_folded(MultiBufferRow(1)));
            assert!(!snapshot.is_line_folded(MultiBufferRow(0)));
            assert_eq!(
                editor.display_text(cx),
                "fn main() {\n    if true ⋯\n    if false {\n        b();\n    }\n}\n",
            );
        });

        cx.update_editor(|editor, window, cx| {
            editor.fold_at(MultiBufferRow(4), window, cx);
        });
        cx.update_editor(|editor, _window, cx| {
            let snapshot = editor.display_snapshot(cx);
            assert!(snapshot.is_line_folded(MultiBufferRow(4)));
            assert_eq!(
                editor.display_text(cx),
                "fn main() {\n    if true ⋯\n    if false ⋯\n}\n",
            );
        });

        cx.update_editor(|editor, window, cx| {
            editor.fold_at(MultiBufferRow(0), window, cx);
        });
        cx.update_editor(|editor, _window, cx| {
            let snapshot = editor.display_snapshot(cx);
            assert!(snapshot.is_line_folded(MultiBufferRow(0)));
            assert_eq!(editor.display_text(cx), "fn main() ⋯\n",);
        });
    }

    #[gpui::test]
    async fn test_lsp_folding_ranges_unsorted_from_server(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        update_test_language_settings(cx, |settings| {
            settings.defaults.lsp_folding_ranges = Some(LspFoldingRanges::On);
        });

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                folding_range_provider: Some(lsp::FoldingRangeProviderCapability::Simple(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let mut folding_request = cx
            .set_request_handler::<lsp::request::FoldingRangeRequest, _, _>(
                move |_, _, _| async move {
                    Ok(Some(vec![
                        FoldingRange {
                            start_line: 6,
                            start_character: Some(11),
                            end_line: 8,
                            end_character: Some(1),
                            kind: None,
                            collapsed_text: None,
                        },
                        FoldingRange {
                            start_line: 0,
                            start_character: Some(10),
                            end_line: 4,
                            end_character: Some(1),
                            kind: None,
                            collapsed_text: None,
                        },
                        FoldingRange {
                            start_line: 1,
                            start_character: Some(13),
                            end_line: 3,
                            end_character: Some(5),
                            kind: None,
                            collapsed_text: None,
                        },
                    ]))
                },
            );

        cx.set_state(
            "ˇfn main() {\n    if true {\n        println!(\"hello\");\n    }\n}\n\nfn other() {\n    let x = 1;\n}\n",
        );
        assert!(folding_request.next().await.is_some());
        cx.run_until_parked();

        cx.editor.read_with(&cx.cx.cx, |editor, cx| {
            assert!(
                editor.lsp_folding_ranges_enabled(cx),
                "Expected LSP folding ranges to be populated despite unsorted server response"
            );
        });

        cx.update_editor(|editor, window, cx| {
            editor.fold_at(MultiBufferRow(0), window, cx);
        });
        cx.update_editor(|editor, _window, cx| {
            assert_eq!(
                editor.display_text(cx),
                "fn main() ⋯\n\nfn other() {\n    let x = 1;\n}\n",
            );
        });

        cx.update_editor(|editor, window, cx| {
            editor.fold_at(MultiBufferRow(6), window, cx);
        });
        cx.update_editor(|editor, _window, cx| {
            assert_eq!(editor.display_text(cx), "fn main() ⋯\n\nfn other() ⋯\n",);
        });
    }
}
