use super::{
    ClickEvent, Context, ConversionConfig, FluentBuilder, FocusHandle, FrameRoot,
    FrameTextInputKind, FrameTextInputSpec, ParentElement, SourceMetadata,
    StatefulInteractiveElement, Styled, Window, apply_output_container, apply_processing_mode, div,
    frame_choice_button, frame_text_input, normalize_output_config, output_container_options,
    output_processing_mode_options, settings_hint_text, settings_section, theme,
};
use crate::{
    app::SizeEstimateStatus, file_queue::format_file_size, size_estimate::SizeEstimateProgress,
};
use frame_core::size_estimate::SizeEstimateMethod;

#[expect(
    clippy::too_many_arguments,
    reason = "The output tab explicitly receives conversion state, editable input, focus, palette, and render context."
)]
pub(in crate::app) fn settings_output_tab(
    config: &ConversionConfig,
    metadata: Option<&SourceMetadata>,
    settings_disabled: bool,
    output_name: &str,
    output_name_focus: Option<&FocusHandle>,
    size_estimate: Option<&SizeEstimateStatus>,
    last_output_bytes: Option<u64>,
    palette: &'static theme::ThemePalette,
    window: &mut Window,
    cx: &mut Context<FrameRoot>,
) -> gpui::Div {
    let dimensions = metadata.and_then(|source| {
        frame_core::dimensions::output_dimensions(
            &crate::conversion_runner::core_config_from_gpui(config),
            source.width?,
            source.height?,
            source.pixel_format.as_deref(),
        )
    });
    div()
        .flex()
        .flex_col()
        .gap_4()
        .child(
            settings_section("Processing mode", palette)
                .child(settings_processing_mode_grid(
                    config,
                    metadata,
                    settings_disabled,
                    palette,
                    window,
                    cx,
                ))
                .child(settings_hint_text(config.processing_mode.hint(), palette)),
        )
        .child(
            settings_section("Output name", palette)
                .child(settings_output_name_field(
                    output_name,
                    settings_disabled,
                    output_name_focus,
                    palette,
                    window,
                    cx,
                ))
                .child(settings_hint_text(
                    "Output is saved to the default folder selected in Settings.",
                    palette,
                )),
        )
        .when_some(dimensions, |panel, (width, height)| {
            panel.child(
                settings_section("Output dimensions", palette)
                    .child(div().child(format!("{width} × {height} px")))
                    .child(settings_hint_text(
                        "Estimated after crop, rotation and scaling.",
                        palette,
                    )),
            )
        })
        .child(settings_size_estimate_section(
            size_estimate,
            last_output_bytes,
            settings_disabled,
            palette,
            window,
            cx,
        ))
        .child(
            settings_section("Output container", palette).child(settings_container_grid(
                config,
                metadata,
                settings_disabled,
                palette,
                window,
                cx,
            )),
        )
}

fn settings_size_estimate_section(
    size_estimate: Option<&SizeEstimateStatus>,
    last_output_bytes: Option<u64>,
    settings_disabled: bool,
    palette: &'static theme::ThemePalette,
    window: &mut Window,
    cx: &mut Context<FrameRoot>,
) -> gpui::Div {
    let is_running = matches!(size_estimate, Some(SizeEstimateStatus::Running(_)));
    let (button_label, button_id) = if is_running {
        ("Cancel", "output-size-estimate-cancel")
    } else {
        ("Estimate size", "output-size-estimate-start")
    };

    settings_section("Estimated file size", palette)
        .when_some(
            size_estimate.and_then(size_estimate_headline),
            |section, headline| section.child(div().child(headline)),
        )
        .child(
            frame_choice_button(
                button_id,
                button_label,
                false,
                is_running || !settings_disabled,
                palette,
                window,
                cx,
            )
            .on_click(cx.listener(move |root, _: &ClickEvent, _window, cx| {
                cx.stop_propagation();
                if is_running {
                    root.cancel_size_estimate();
                } else if !settings_disabled {
                    root.start_size_estimate(cx);
                }
                cx.notify();
            })),
        )
        .child(settings_hint_text(
            size_estimate.map_or_else(
                || "Test-encodes parts of the video with these settings.".to_string(),
                size_estimate_hint,
            ),
            palette,
        ))
        .when_some(last_output_bytes, |section, bytes| {
            section.child(settings_hint_text(
                format!("Last output: {}", format_file_size(bytes)),
                palette,
            ))
        })
}

fn size_estimate_headline(status: &SizeEstimateStatus) -> Option<String> {
    let SizeEstimateStatus::Ready(estimate) = status else {
        return None;
    };
    Some(if estimate.low_bytes == estimate.high_bytes {
        format_file_size(estimate.bytes)
    } else {
        format!(
            "≈ {} ({} – {})",
            format_file_size(estimate.bytes),
            format_file_size(estimate.low_bytes),
            format_file_size(estimate.high_bytes)
        )
    })
}

fn size_estimate_hint(status: &SizeEstimateStatus) -> String {
    match status {
        SizeEstimateStatus::Running(SizeEstimateProgress::Preparing) => {
            "Reading the source…".to_string()
        }
        SizeEstimateStatus::Running(SizeEstimateProgress::EncodingFull) => {
            "Encoding the whole clip…".to_string()
        }
        SizeEstimateStatus::Running(SizeEstimateProgress::ScanningSource) => {
            "Scanning the whole video…".to_string()
        }
        SizeEstimateStatus::Running(SizeEstimateProgress::EncodingSample { current, total }) => {
            format!("Test-encoding section {current} of {total}…")
        }
        SizeEstimateStatus::Failed(error) => format!("Could not estimate the size: {error}"),
        SizeEstimateStatus::Ready(estimate) => match estimate.method {
            SizeEstimateMethod::FullEncode => {
                "Exact size: the clip was encoded in full with these settings.".to_string()
            }
            SizeEstimateMethod::SourcePackets => {
                "Added up from the source streams this copy keeps.".to_string()
            }
            SizeEstimateMethod::FixedBitrate => {
                "Calculated from the bitrate. Simple footage can come out smaller.".to_string()
            }
            SizeEstimateMethod::Sampled { samples } => {
                format!("Test-encoded {samples} sections and scanned the whole video.")
            }
        },
    }
}

pub(in crate::app) fn settings_processing_mode_grid(
    config: &ConversionConfig,
    metadata: Option<&SourceMetadata>,
    settings_disabled: bool,
    palette: &'static theme::ThemePalette,
    window: &mut Window,
    cx: &mut Context<FrameRoot>,
) -> gpui::Div {
    let mut grid = div().grid().grid_cols(2).gap_2();
    for option in output_processing_mode_options(config, metadata, settings_disabled) {
        let mode = option.mode;
        let is_enabled = !option.is_disabled;
        grid = grid.child(
            frame_choice_button(
                format!("output-mode-{}", option.mode.id()),
                option.label,
                option.is_selected,
                is_enabled,
                palette,
                window,
                cx,
            )
            .on_click(cx.listener(move |root, _: &ClickEvent, _window, cx| {
                cx.stop_propagation();
                if !is_enabled {
                    return;
                }

                let metadata = root.selected_source_metadata();
                if root.update_selected_config(|config| {
                    apply_processing_mode(config, metadata.as_ref(), mode)
                }) {
                    root.resolve_selected_settings_tab(metadata.as_ref());
                    cx.notify();
                }
            })),
        );
    }
    grid
}

pub(in crate::app) fn settings_container_grid(
    config: &ConversionConfig,
    metadata: Option<&SourceMetadata>,
    settings_disabled: bool,
    palette: &'static theme::ThemePalette,
    window: &mut Window,
    cx: &mut Context<FrameRoot>,
) -> gpui::Div {
    let mut grid = div().grid().grid_cols(2).gap_2();
    for option in output_container_options(config, metadata, settings_disabled) {
        let container = option.container;
        let is_enabled = !option.is_disabled;
        grid = grid.child(
            frame_choice_button(
                format!("output-container-{container}"),
                &container,
                option.is_selected,
                is_enabled,
                palette,
                window,
                cx,
            )
            .on_click(cx.listener(move |root, _: &ClickEvent, _window, cx| {
                cx.stop_propagation();
                if !is_enabled {
                    return;
                }

                let metadata = root.selected_source_metadata();
                let changed = root.update_selected_config(|config| {
                    apply_output_container(config, &container)
                        | normalize_output_config(config, metadata.as_ref())
                });
                if changed {
                    root.resolve_selected_settings_tab(metadata.as_ref());
                    cx.notify();
                }
            })),
        );
    }
    grid
}

pub(in crate::app) fn settings_output_name_field(
    output_name: &str,
    disabled: bool,
    output_name_focus: Option<&FocusHandle>,
    palette: &'static theme::ThemePalette,
    window: &Window,
    cx: &Context<FrameRoot>,
) -> gpui::Stateful<gpui::Div> {
    frame_text_input(
        FrameTextInputSpec {
            id: "settings-output-name-field",
            value: output_name,
            placeholder: "my_render_final",
            disabled,
            focus: output_name_focus,
            kind: FrameTextInputKind::OutputName,
        },
        palette,
        window,
        cx,
    )
}
