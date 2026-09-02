use eframe::egui::{
    Grid, Label, RichText, ScrollArea, TextStyle, Ui, containers::scroll_area::ScrollBarVisibility,
    style::ScrollStyle,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag};

const MIN_TABLE_COLUMN_WIDTH: f32 = 120.0;
const TABLE_COLUMN_SPACING: f32 = 12.0;
const TABLE_ROW_SPACING: f32 = 8.0;

/// Draws model-authored Markdown without changing the conversation text that is stored on disk.
///
/// Raw HTML is deliberately rendered as text. Assistant output is local-model output, and the
/// native client has no need to interpret arbitrary HTML in order to support CommonMark/GFM.
pub fn show(ui: &mut Ui, markdown: &str) {
    let Node::Root { children } = parse(markdown) else {
        return;
    };
    let mut next_table_id = 0;
    render_children(ui, &children, 0, &mut next_table_id);
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct InlineStyle {
    emphasis: bool,
    strong: bool,
    strikethrough: bool,
    link: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Inline {
    text: String,
    style: InlineStyle,
    code: bool,
}

#[derive(Debug)]
enum Node {
    Root {
        children: Vec<Node>,
    },
    Paragraph {
        content: Vec<Inline>,
    },
    Heading {
        level: HeadingLevel,
        content: Vec<Inline>,
    },
    BlockQuote {
        children: Vec<Node>,
    },
    List {
        start: Option<u64>,
        items: Vec<Node>,
    },
    Item {
        content: Vec<Inline>,
        children: Vec<Node>,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    HtmlBlock {
        content: Vec<Inline>,
    },
    Table {
        children: Vec<Node>,
    },
    TableHead {
        cells: Vec<Node>,
    },
    TableRow {
        cells: Vec<Node>,
    },
    TableCell {
        content: Vec<Inline>,
    },
    Rule,
}

impl Node {
    fn push_child(&mut self, child: Node) {
        match self {
            Self::Root { children }
            | Self::BlockQuote { children }
            | Self::Item { children, .. }
            | Self::Table { children } => children.push(child),
            Self::List { items, .. } => items.push(child),
            Self::TableHead { cells } | Self::TableRow { cells } => cells.push(child),
            _ => {}
        }
    }

    fn push_inline(&mut self, inline: Inline) -> bool {
        let content = match self {
            Self::Paragraph { content }
            | Self::Heading { content, .. }
            | Self::Item { content, .. }
            | Self::HtmlBlock { content }
            | Self::TableCell { content } => content,
            _ => return false,
        };

        if let Some(last) = content.last_mut()
            && last.style == inline.style
            && last.code == inline.code
        {
            last.text.push_str(&inline.text);
        } else {
            content.push(inline);
        }
        true
    }
}

enum OpenElement {
    Block,
    Inline,
    Ignored,
}

enum InlineChange {
    Emphasis,
    Strong,
    Strikethrough,
    Link(String),
}

fn parse(markdown: &str) -> Node {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_GFM);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    let mut nodes = vec![Node::Root {
        children: Vec::new(),
    }];
    let mut open_elements = Vec::new();
    let mut styles = vec![InlineStyle::default()];

    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(tag) => {
                if let Some(change) = inline_change(&tag) {
                    let mut next = styles.last().cloned().unwrap_or_default();
                    apply_inline_change(&mut next, change);
                    styles.push(next);
                    open_elements.push(OpenElement::Inline);
                } else if let Some(node) = block_for_tag(&tag) {
                    nodes.push(node);
                    open_elements.push(OpenElement::Block);
                } else {
                    open_elements.push(OpenElement::Ignored);
                }
            }
            Event::End(_) => match open_elements.pop() {
                Some(OpenElement::Block) => close_block(&mut nodes),
                Some(OpenElement::Inline) => {
                    if styles.len() > 1 {
                        styles.pop();
                    }
                }
                Some(OpenElement::Ignored) | None => {}
            },
            Event::Text(value) | Event::Html(value) | Event::InlineHtml(value) => {
                append_text(
                    &mut nodes,
                    value.as_ref(),
                    styles.last().cloned().unwrap_or_default(),
                    false,
                );
            }
            Event::Code(value) | Event::InlineMath(value) | Event::DisplayMath(value) => {
                append_text(
                    &mut nodes,
                    value.as_ref(),
                    styles.last().cloned().unwrap_or_default(),
                    true,
                );
            }
            Event::SoftBreak | Event::HardBreak => {
                append_text(
                    &mut nodes,
                    "\n",
                    styles.last().cloned().unwrap_or_default(),
                    false,
                );
            }
            Event::TaskListMarker(checked) => {
                append_text(
                    &mut nodes,
                    if checked { "☑ " } else { "☐ " },
                    styles.last().cloned().unwrap_or_default(),
                    false,
                );
            }
            Event::FootnoteReference(label) => {
                append_text(
                    &mut nodes,
                    &format!("[^{label}]"),
                    styles.last().cloned().unwrap_or_default(),
                    false,
                );
            }
            Event::Rule => {
                if let Some(node) = nodes.last_mut() {
                    node.push_child(Node::Rule);
                }
            }
        }
    }

    while nodes.len() > 1 {
        close_block(&mut nodes);
    }

    nodes.pop().unwrap_or(Node::Root {
        children: Vec::new(),
    })
}

fn inline_change(tag: &Tag<'_>) -> Option<InlineChange> {
    match tag {
        Tag::Emphasis => Some(InlineChange::Emphasis),
        Tag::Strong => Some(InlineChange::Strong),
        Tag::Strikethrough => Some(InlineChange::Strikethrough),
        Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
            Some(InlineChange::Link(dest_url.to_string()))
        }
        _ => None,
    }
}

fn apply_inline_change(style: &mut InlineStyle, change: InlineChange) {
    match change {
        InlineChange::Emphasis => style.emphasis = true,
        InlineChange::Strong => style.strong = true,
        InlineChange::Strikethrough => style.strikethrough = true,
        InlineChange::Link(url) => style.link = Some(url),
    }
}

fn block_for_tag(tag: &Tag<'_>) -> Option<Node> {
    match tag {
        Tag::Paragraph => Some(Node::Paragraph {
            content: Vec::new(),
        }),
        Tag::Heading { level, .. } => Some(Node::Heading {
            level: *level,
            content: Vec::new(),
        }),
        Tag::BlockQuote(_)
        | Tag::FootnoteDefinition(_)
        | Tag::DefinitionList
        | Tag::DefinitionListDefinition => Some(Node::BlockQuote {
            children: Vec::new(),
        }),
        Tag::DefinitionListTitle => Some(Node::Paragraph {
            content: Vec::new(),
        }),
        Tag::CodeBlock(kind) => Some(Node::CodeBlock {
            language: match kind {
                CodeBlockKind::Fenced(language) if !language.is_empty() => {
                    Some(language.to_string())
                }
                _ => None,
            },
            code: String::new(),
        }),
        Tag::HtmlBlock => Some(Node::HtmlBlock {
            content: Vec::new(),
        }),
        Tag::List(start) => Some(Node::List {
            start: *start,
            items: Vec::new(),
        }),
        Tag::Item => Some(Node::Item {
            content: Vec::new(),
            children: Vec::new(),
        }),
        Tag::Table(_) => Some(Node::Table {
            children: Vec::new(),
        }),
        Tag::TableHead => Some(Node::TableHead { cells: Vec::new() }),
        Tag::TableRow => Some(Node::TableRow { cells: Vec::new() }),
        Tag::TableCell => Some(Node::TableCell {
            content: Vec::new(),
        }),
        _ => None,
    }
}

fn append_text(nodes: &mut [Node], text: &str, style: InlineStyle, code: bool) {
    if let Some(Node::CodeBlock { code: block, .. }) = nodes.last_mut() {
        block.push_str(text);
        return;
    }

    let inline = Inline {
        text: text.to_owned(),
        style,
        code,
    };
    for node in nodes.iter_mut().rev() {
        if node.push_inline(inline.clone()) {
            return;
        }
    }
}

fn close_block(nodes: &mut Vec<Node>) {
    if nodes.len() <= 1 {
        return;
    }
    let Some(node) = nodes.pop() else {
        return;
    };
    if let Some(parent) = nodes.last_mut() {
        parent.push_child(node);
    }
}

#[derive(Clone, Copy, Default)]
struct InlinePresentation {
    font_size: Option<f32>,
    force_strong: bool,
}

fn render_children(ui: &mut Ui, children: &[Node], list_depth: usize, next_table_id: &mut usize) {
    for (index, child) in children.iter().enumerate() {
        if index > 0 {
            ui.add_space(7.0);
        }
        render_block(ui, child, list_depth, next_table_id);
    }
}

fn render_block(ui: &mut Ui, node: &Node, list_depth: usize, next_table_id: &mut usize) {
    match node {
        Node::Root { children } => render_children(ui, children, list_depth, next_table_id),
        Node::Paragraph { content } | Node::HtmlBlock { content } => {
            render_inlines(ui, content, InlinePresentation::default());
        }
        Node::Heading { level, content } => render_inlines(
            ui,
            content,
            InlinePresentation {
                font_size: Some(heading_size(ui, *level)),
                force_strong: true,
            },
        ),
        Node::BlockQuote { children } => {
            ui.group(|ui| {
                ui.horizontal_top(|ui| {
                    ui.add_space(5.0);
                    ui.vertical(|ui| render_children(ui, children, list_depth, next_table_id));
                });
            });
        }
        Node::List { start, items } => {
            render_list(ui, *start, items, list_depth, next_table_id);
        }
        Node::Item { content, children } => {
            if !content.is_empty() {
                render_inlines(ui, content, InlinePresentation::default());
            }
            render_children(ui, children, list_depth + 1, next_table_id);
        }
        Node::CodeBlock { language, code } => {
            ui.group(|ui| {
                if let Some(language) = language {
                    ui.label(RichText::new(language).small().weak());
                }
                ui.add(Label::new(RichText::new(code).monospace()).wrap());
            });
        }
        Node::Table { children } => render_table(ui, children, list_depth, next_table_id),
        Node::TableHead { cells } | Node::TableRow { cells } => {
            render_children(ui, cells, list_depth, next_table_id);
        }
        Node::TableCell { content } => render_inlines(ui, content, InlinePresentation::default()),
        Node::Rule => {
            ui.separator();
        }
    }
}

fn render_list(
    ui: &mut Ui,
    start: Option<u64>,
    items: &[Node],
    list_depth: usize,
    next_table_id: &mut usize,
) {
    for (index, item) in items.iter().enumerate() {
        let marker = start
            .map(|first| format!("{}. ", first + index as u64))
            .unwrap_or_else(|| "• ".to_owned());
        ui.horizontal_top(|ui| {
            ui.add_space(list_depth as f32 * 14.0);
            ui.label(marker);
            ui.vertical(|ui| match item {
                Node::Item { content, children } => {
                    if !content.is_empty() {
                        render_inlines(ui, content, InlinePresentation::default());
                    }
                    render_children(ui, children, list_depth + 1, next_table_id);
                }
                other => render_block(ui, other, list_depth + 1, next_table_id),
            });
        });
        if index + 1 < items.len() {
            ui.add_space(3.0);
        }
    }
}

fn render_table(ui: &mut Ui, children: &[Node], list_depth: usize, next_table_id: &mut usize) {
    let id = ui.id().with(("markdown-table", *next_table_id));
    *next_table_id += 1;

    let column_count = children
        .iter()
        .filter_map(|row| match row {
            Node::TableHead { cells } | Node::TableRow { cells } => Some(cells.len()),
            _ => None,
        })
        .max()
        .unwrap_or(0);

    if column_count == 0 {
        return;
    }

    ui.group(|ui| {
        ui.take_available_width();
        let available_width = ui.available_width();
        let layout = responsive_table_layout(available_width, column_count);

        let scroll_bar_visibility = if layout.needs_horizontal_scroll {
            ui.style_mut().spacing.scroll = ScrollStyle::solid();
            ScrollBarVisibility::AlwaysVisible
        } else {
            ScrollBarVisibility::VisibleWhenNeeded
        };

        ScrollArea::horizontal()
            .id_salt(id.with("horizontal-scroll"))
            .max_width(available_width)
            .auto_shrink([false, true])
            .scroll_bar_visibility(scroll_bar_visibility)
            .show(ui, |ui| {
                ui.set_width(layout.table_width);
                Grid::new(id)
                    .num_columns(column_count)
                    .min_col_width(layout.column_width)
                    .max_col_width(layout.column_width)
                    .spacing([TABLE_COLUMN_SPACING, TABLE_ROW_SPACING])
                    .striped(true)
                    .show(ui, |ui| {
                        for row in children {
                            let (header, cells) = match row {
                                Node::TableHead { cells } => (true, cells.as_slice()),
                                Node::TableRow { cells } => (false, cells.as_slice()),
                                _ => continue,
                            };
                            for cell in cells {
                                match cell {
                                    Node::TableCell { content } => render_inlines(
                                        ui,
                                        content,
                                        InlinePresentation {
                                            force_strong: header,
                                            ..Default::default()
                                        },
                                    ),
                                    other => render_block(ui, other, list_depth, next_table_id),
                                }
                            }
                            ui.end_row();
                        }
                    });
            });
    });
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ResponsiveTableLayout {
    column_width: f32,
    table_width: f32,
    needs_horizontal_scroll: bool,
}

fn responsive_table_layout(available_width: f32, column_count: usize) -> ResponsiveTableLayout {
    let column_count = column_count.max(1);
    let spacing_width = TABLE_COLUMN_SPACING * column_count.saturating_sub(1) as f32;
    let responsive_column_width = ((available_width.max(0.0) - spacing_width).max(0.0)
        / column_count as f32)
        .max(MIN_TABLE_COLUMN_WIDTH);

    let table_width = responsive_column_width * column_count as f32 + spacing_width;

    ResponsiveTableLayout {
        column_width: responsive_column_width,
        table_width,
        needs_horizontal_scroll: table_width > available_width.max(0.0),
    }
}

fn render_inlines(ui: &mut Ui, content: &[Inline], presentation: InlinePresentation) {
    ui.horizontal_wrapped(|ui| {
        let original_spacing = ui.spacing().item_spacing;
        ui.spacing_mut().item_spacing.x = 0.0;
        for inline in content {
            let mut text = RichText::new(&inline.text);
            if inline.code {
                text = text.monospace();
            }
            if inline.style.emphasis {
                text = text.italics();
            }
            if inline.style.strong || presentation.force_strong {
                text = text.strong();
            }
            if inline.style.strikethrough {
                text = text.strikethrough();
            }
            if let Some(font_size) = presentation.font_size {
                text = text.size(font_size);
            }

            if let Some(url) = inline.style.link.as_deref() {
                ui.hyperlink_to(text, url);
            } else {
                ui.add(Label::new(text).wrap());
            }
        }
        ui.spacing_mut().item_spacing = original_spacing;
    });
}

fn heading_size(ui: &Ui, level: HeadingLevel) -> f32 {
    let body_size = ui
        .style()
        .text_styles
        .get(&TextStyle::Body)
        .map(|font| font.size)
        .unwrap_or(14.0);
    let multiplier = match level {
        HeadingLevel::H1 => 1.65,
        HeadingLevel::H2 => 1.4,
        HeadingLevel::H3 => 1.2,
        HeadingLevel::H4 => 1.1,
        HeadingLevel::H5 | HeadingLevel::H6 => 1.0,
    };
    body_size * multiplier
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_assistant_markdown_into_semantic_blocks() {
        let document = parse(
            "## 更新\n\n**確認済み**の[出典](https://example.com)です。\n\n- 一つ目\n- `二つ目`",
        );
        let Node::Root { children } = document else {
            panic!("root document expected");
        };

        assert!(matches!(
            children.first(),
            Some(Node::Heading {
                level: HeadingLevel::H2,
                ..
            })
        ));
        let Some(Node::Paragraph { content }) = children.get(1) else {
            panic!("paragraph expected");
        };
        assert!(content.iter().any(|inline| inline.style.strong));
        assert!(
            content
                .iter()
                .any(|inline| inline.style.link.as_deref() == Some("https://example.com"))
        );
        assert!(matches!(children.get(2), Some(Node::List { .. })));
    }

    #[test]
    fn parses_and_draws_gfm_tables() {
        let markdown = "| 項目 | 状態 |\n| --- | --- |\n| **Taceta** | `ready` |";
        let document = parse(markdown);
        let Node::Root { children } = &document else {
            panic!("root document expected");
        };
        let Some(Node::Table { children }) = children.first() else {
            panic!("table expected");
        };
        assert!(matches!(children.first(), Some(Node::TableHead { .. })));
        assert!(matches!(children.get(1), Some(Node::TableRow { .. })));

        eframe::egui::__run_test_ui(|ui| show(ui, markdown));
    }

    #[test]
    fn table_columns_follow_available_width_and_keep_a_readable_minimum() {
        let wide = responsive_table_layout(900.0, 3);
        assert_eq!(wide.table_width, 900.0);
        assert!(wide.column_width > 290.0);
        assert!(!wide.needs_horizontal_scroll);

        let wider = responsive_table_layout(1_200.0, 3);
        assert!(wider.column_width > wide.column_width);
        assert_eq!(wider.table_width, 1_200.0);
        assert!(!wider.needs_horizontal_scroll);

        let narrow = responsive_table_layout(240.0, 3);
        assert_eq!(narrow.column_width, MIN_TABLE_COLUMN_WIDTH);
        assert!(narrow.table_width > 240.0);
        assert!(narrow.needs_horizontal_scroll);
    }
}
