use std::{collections::HashMap, sync::OnceLock};

use eframe::egui::{
    Align, Color32, FontFamily, FontId, Grid, Label, Layout, RichText, ScrollArea, TextFormat,
    TextStyle, TextWrapMode, Ui, Vec2, containers::scroll_area::ScrollBarVisibility,
    style::ScrollStyle, text::LayoutJob,
};
use pulldown_cmark::{
    Alignment as MarkdownAlignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag,
};
use syntect::{
    easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet, util::LinesWithEndings,
};

const MIN_TABLE_COLUMN_WIDTH: f32 = 120.0;
const TABLE_COLUMN_SPACING: f32 = 12.0;
const TABLE_ROW_SPACING: f32 = 8.0;

/// Draws model-authored Markdown without changing the conversation text that is stored on disk.
///
/// Arbitrary HTML is deliberately never executed. A small presentation-only allowlist is mapped
/// to native egui styling so model output can use common `<strong>`, `<em>`, `<del>`, and `<u>`
/// tags without creating a browser or script surface.
pub fn show(ui: &mut Ui, markdown: &str) {
    let Node::Root { children } = parse(markdown) else {
        return;
    };
    let mut context = RenderContext::for_document(&children);
    render_children(ui, &children, 0, &mut context);
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct InlineStyle {
    emphasis: bool,
    strong: bool,
    strikethrough: bool,
    underline: bool,
    link: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum InlineKind {
    Text(String),
    Code(String),
    HardBreak,
    InlineMath(String),
    DisplayMath(String),
    Image {
        destination: String,
        title: String,
        alt: String,
    },
    FootnoteReference(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Inline {
    kind: InlineKind,
    style: InlineStyle,
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
        task: Option<bool>,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    HtmlBlock {
        content: Vec<Inline>,
    },
    FootnoteDefinition {
        label: String,
        content: Vec<Inline>,
        children: Vec<Node>,
    },
    DefinitionList {
        children: Vec<Node>,
    },
    DefinitionTerm {
        content: Vec<Inline>,
    },
    DefinitionDescription {
        content: Vec<Inline>,
        children: Vec<Node>,
    },
    Table {
        alignments: Vec<MarkdownAlignment>,
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
            | Self::FootnoteDefinition { children, .. }
            | Self::DefinitionList { children }
            | Self::DefinitionDescription { children, .. }
            | Self::Table { children, .. } => children.push(child),
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
            | Self::FootnoteDefinition { content, .. }
            | Self::DefinitionTerm { content }
            | Self::DefinitionDescription { content, .. }
            | Self::TableCell { content } => content,
            _ => return false,
        };

        if let Some(last) = content.last_mut()
            && last.style == inline.style
            && let (InlineKind::Text(last_text), InlineKind::Text(next_text)) =
                (&mut last.kind, &inline.kind)
        {
            last_text.push_str(next_text);
        } else if let Some(last) = content.last_mut()
            && last.style == inline.style
            && let (InlineKind::Code(last_text), InlineKind::Code(next_text)) =
                (&mut last.kind, &inline.kind)
        {
            last_text.push_str(next_text);
        } else {
            content.push(inline);
        }
        true
    }
}

enum OpenElement {
    Block,
    Inline,
    Image {
        destination: String,
        title: String,
        alt: String,
    },
    Ignored,
}

enum InlineChange {
    Emphasis,
    Strong,
    Strikethrough,
    Underline,
    Link(String),
}

fn parse(markdown: &str) -> Node {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_GFM);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_MATH);
    options.insert(Options::ENABLE_DEFINITION_LIST);

    let mut nodes = vec![Node::Root {
        children: Vec::new(),
    }];
    let mut open_elements = Vec::new();
    let mut styles = vec![InlineStyle::default()];

    let normalized = normalize_math_delimiters(markdown);
    for event in Parser::new_ext(&normalized, options) {
        match event {
            Event::Start(tag) => {
                if let Tag::Image {
                    dest_url, title, ..
                } = &tag
                {
                    open_elements.push(OpenElement::Image {
                        destination: dest_url.to_string(),
                        title: title.to_string(),
                        alt: String::new(),
                    });
                } else if let Some(change) = inline_change(&tag) {
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
                Some(OpenElement::Image {
                    destination,
                    title,
                    alt,
                }) => append_inline(
                    &mut nodes,
                    InlineKind::Image {
                        destination,
                        title,
                        alt,
                    },
                    styles.last().cloned().unwrap_or_default(),
                ),
                Some(OpenElement::Ignored) | None => {}
            },
            Event::Text(value) => {
                if !append_image_alt(&mut open_elements, value.as_ref()) {
                    append_inline(
                        &mut nodes,
                        InlineKind::Text(value.to_string()),
                        styles.last().cloned().unwrap_or_default(),
                    );
                }
            }
            Event::Html(value) | Event::InlineHtml(value) => {
                append_safe_html(&mut nodes, &mut styles, value.as_ref());
            }
            Event::Code(value) => {
                if !append_image_alt(&mut open_elements, value.as_ref()) {
                    append_inline(
                        &mut nodes,
                        InlineKind::Code(value.to_string()),
                        styles.last().cloned().unwrap_or_default(),
                    );
                }
            }
            Event::InlineMath(value) => {
                append_inline(
                    &mut nodes,
                    InlineKind::InlineMath(value.to_string()),
                    styles.last().cloned().unwrap_or_default(),
                );
            }
            Event::DisplayMath(value) => {
                append_inline(
                    &mut nodes,
                    InlineKind::DisplayMath(value.to_string()),
                    styles.last().cloned().unwrap_or_default(),
                );
            }
            Event::SoftBreak => {
                append_inline(
                    &mut nodes,
                    InlineKind::Text(" ".to_owned()),
                    styles.last().cloned().unwrap_or_default(),
                );
            }
            Event::HardBreak => append_inline(
                &mut nodes,
                InlineKind::HardBreak,
                styles.last().cloned().unwrap_or_default(),
            ),
            Event::TaskListMarker(checked) => {
                mark_task_item(&mut nodes, checked);
            }
            Event::FootnoteReference(label) => {
                append_inline(
                    &mut nodes,
                    InlineKind::FootnoteReference(label.to_string()),
                    styles.last().cloned().unwrap_or_default(),
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
        Tag::Link { dest_url, .. } => Some(InlineChange::Link(dest_url.to_string())),
        _ => None,
    }
}

fn apply_inline_change(style: &mut InlineStyle, change: InlineChange) {
    match change {
        InlineChange::Emphasis => style.emphasis = true,
        InlineChange::Strong => style.strong = true,
        InlineChange::Strikethrough => style.strikethrough = true,
        InlineChange::Underline => style.underline = true,
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
        Tag::BlockQuote(_) => Some(Node::BlockQuote {
            children: Vec::new(),
        }),
        Tag::FootnoteDefinition(label) => Some(Node::FootnoteDefinition {
            label: label.to_string(),
            content: Vec::new(),
            children: Vec::new(),
        }),
        Tag::DefinitionList => Some(Node::DefinitionList {
            children: Vec::new(),
        }),
        Tag::DefinitionListTitle => Some(Node::DefinitionTerm {
            content: Vec::new(),
        }),
        Tag::DefinitionListDefinition => Some(Node::DefinitionDescription {
            content: Vec::new(),
            children: Vec::new(),
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
            task: None,
        }),
        Tag::Table(alignments) => Some(Node::Table {
            alignments: alignments.clone(),
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

fn append_inline(nodes: &mut [Node], kind: InlineKind, style: InlineStyle) {
    if let Some(Node::CodeBlock { code: block, .. }) = nodes.last_mut()
        && let InlineKind::Text(text) | InlineKind::Code(text) = &kind
    {
        block.push_str(text);
        return;
    }

    let inline = Inline { kind, style };
    for node in nodes.iter_mut().rev() {
        if node.push_inline(inline.clone()) {
            return;
        }
    }
}

fn append_image_alt(open_elements: &mut [OpenElement], text: &str) -> bool {
    if let Some(OpenElement::Image { alt, .. }) = open_elements
        .iter_mut()
        .rev()
        .find(|element| matches!(element, OpenElement::Image { .. }))
    {
        alt.push_str(text);
        true
    } else {
        false
    }
}

fn mark_task_item(nodes: &mut [Node], checked: bool) {
    if let Some(Node::Item { task, .. }) = nodes
        .iter_mut()
        .rev()
        .find(|node| matches!(node, Node::Item { .. }))
    {
        *task = Some(checked);
    }
}

fn normalize_math_delimiters(markdown: &str) -> String {
    let mut normalized = String::with_capacity(markdown.len());
    let mut fence: Option<(u8, usize)> = None;

    for line in markdown.split_inclusive('\n') {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let marker = trimmed.as_bytes().first().copied();
        let marker_len = marker
            .filter(|marker| matches!(marker, b'`' | b'~'))
            .map(|marker| trimmed.bytes().take_while(|byte| *byte == marker).count())
            .unwrap_or(0);

        if indent <= 3 && marker_len >= 3 {
            match fence {
                None => fence = Some((marker.unwrap_or_default(), marker_len)),
                Some((open_marker, open_len))
                    if marker == Some(open_marker) && marker_len >= open_len =>
                {
                    fence = None;
                }
                _ => {}
            }
            normalized.push_str(line);
        } else if fence.is_some() {
            normalized.push_str(line);
        } else {
            normalized.push_str(&normalize_math_delimiters_in_line(line));
        }
    }

    normalized
}

fn normalize_math_delimiters_in_line(line: &str) -> String {
    let mut normalized = String::with_capacity(line.len());
    let mut index = 0;
    let mut inline_code_run: Option<usize> = None;

    while index < line.len() {
        let remaining = &line[index..];
        if remaining.starts_with('`') {
            let run = remaining.bytes().take_while(|byte| *byte == b'`').count();
            match inline_code_run {
                None => inline_code_run = Some(run),
                Some(open) if open == run => inline_code_run = None,
                _ => {}
            }
            normalized.push_str(&remaining[..run]);
            index += run;
        } else if inline_code_run.is_none() && remaining.starts_with("\\(") {
            if let Some(close) = remaining[2..].find("\\)") {
                normalized.push('$');
                normalized.push_str(remaining[2..2 + close].trim());
                normalized.push('$');
                index += 2 + close + 2;
            } else {
                normalized.push_str("\\(");
                index += 2;
            }
        } else if inline_code_run.is_none() && remaining.starts_with("\\)") {
            normalized.push('$');
            index += 2;
        } else if inline_code_run.is_none() && remaining.starts_with("\\[") {
            normalized.push_str("$$");
            index += 2;
        } else if inline_code_run.is_none() && remaining.starts_with("\\]") {
            normalized.push_str("$$");
            index += 2;
        } else {
            let character = remaining.chars().next().unwrap_or_default();
            normalized.push(character);
            index += character.len_utf8();
        }
    }

    normalized
}

fn append_safe_html(nodes: &mut [Node], styles: &mut Vec<InlineStyle>, fragment: &str) {
    let mut remaining = fragment;
    while let Some(open) = remaining.find('<') {
        if open > 0 {
            append_inline(
                nodes,
                InlineKind::Text(remaining[..open].to_owned()),
                styles.last().cloned().unwrap_or_default(),
            );
        }

        let Some(relative_close) = remaining[open..].find('>') else {
            append_inline(
                nodes,
                InlineKind::Text(remaining[open..].to_owned()),
                styles.last().cloned().unwrap_or_default(),
            );
            return;
        };
        let close = open + relative_close;
        let literal = &remaining[open..=close];
        let tag = remaining[open + 1..close].trim().to_ascii_lowercase();
        let closing = tag.starts_with('/');
        let name = tag
            .trim_start_matches('/')
            .trim_end_matches('/')
            .split_ascii_whitespace()
            .next()
            .unwrap_or_default();

        let change = match name {
            "strong" | "b" => Some(InlineChange::Strong),
            "em" | "i" => Some(InlineChange::Emphasis),
            "del" | "s" | "strike" => Some(InlineChange::Strikethrough),
            "u" => Some(InlineChange::Underline),
            _ => None,
        };

        if name == "br" && !closing {
            append_inline(
                nodes,
                InlineKind::HardBreak,
                styles.last().cloned().unwrap_or_default(),
            );
        } else if let Some(change) = change {
            if closing {
                if styles.len() > 1 {
                    styles.pop();
                }
            } else {
                let mut next = styles.last().cloned().unwrap_or_default();
                apply_inline_change(&mut next, change);
                styles.push(next);
            }
        } else {
            append_inline(
                nodes,
                InlineKind::Text(literal.to_owned()),
                styles.last().cloned().unwrap_or_default(),
            );
        }

        remaining = &remaining[close + 1..];
    }

    if !remaining.is_empty() {
        append_inline(
            nodes,
            InlineKind::Text(remaining.to_owned()),
            styles.last().cloned().unwrap_or_default(),
        );
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

struct RenderContext {
    next_table_id: usize,
    footnote_numbers: HashMap<String, usize>,
}

impl RenderContext {
    fn for_document(children: &[Node]) -> Self {
        let mut footnote_numbers = HashMap::new();
        let mut next = 1;
        collect_footnote_references(children, &mut footnote_numbers, &mut next);
        collect_footnote_definitions(children, &mut footnote_numbers, &mut next);
        Self {
            next_table_id: 0,
            footnote_numbers,
        }
    }

    fn footnote_number(&self, label: &str) -> String {
        self.footnote_numbers
            .get(label)
            .map(usize::to_string)
            .unwrap_or_else(|| label.to_owned())
    }
}

fn collect_footnote_references(
    nodes: &[Node],
    numbers: &mut HashMap<String, usize>,
    next: &mut usize,
) {
    for node in nodes {
        for inline in node_inlines(node) {
            if let InlineKind::FootnoteReference(label) = &inline.kind
                && !numbers.contains_key(label)
            {
                numbers.insert(label.clone(), *next);
                *next += 1;
            }
        }
        collect_footnote_references(node_children(node), numbers, next);
    }
}

fn collect_footnote_definitions(
    nodes: &[Node],
    numbers: &mut HashMap<String, usize>,
    next: &mut usize,
) {
    for node in nodes {
        if let Node::FootnoteDefinition { label, .. } = node
            && !numbers.contains_key(label)
        {
            numbers.insert(label.clone(), *next);
            *next += 1;
        }
        collect_footnote_definitions(node_children(node), numbers, next);
    }
}

fn node_inlines(node: &Node) -> &[Inline] {
    match node {
        Node::Paragraph { content }
        | Node::Heading { content, .. }
        | Node::Item { content, .. }
        | Node::HtmlBlock { content }
        | Node::FootnoteDefinition { content, .. }
        | Node::DefinitionTerm { content }
        | Node::DefinitionDescription { content, .. }
        | Node::TableCell { content } => content,
        _ => &[],
    }
}

fn node_children(node: &Node) -> &[Node] {
    match node {
        Node::Root { children }
        | Node::BlockQuote { children }
        | Node::Item { children, .. }
        | Node::FootnoteDefinition { children, .. }
        | Node::DefinitionList { children }
        | Node::DefinitionDescription { children, .. }
        | Node::Table { children, .. } => children,
        Node::List { items, .. } => items,
        Node::TableHead { cells } | Node::TableRow { cells } => cells,
        _ => &[],
    }
}

fn render_children(ui: &mut Ui, children: &[Node], list_depth: usize, context: &mut RenderContext) {
    for (index, child) in children.iter().enumerate() {
        if index > 0 {
            ui.add_space(7.0);
        }
        render_block(ui, child, list_depth, context);
    }
}

fn render_block(ui: &mut Ui, node: &Node, list_depth: usize, context: &mut RenderContext) {
    match node {
        Node::Root { children } => render_children(ui, children, list_depth, context),
        Node::Paragraph { content } | Node::HtmlBlock { content } => {
            render_inlines(ui, content, InlinePresentation::default(), context);
        }
        Node::Heading { level, content } => render_inlines(
            ui,
            content,
            InlinePresentation {
                font_size: Some(heading_size(ui, *level)),
                force_strong: true,
            },
            context,
        ),
        Node::BlockQuote { children } => {
            ui.group(|ui| {
                ui.horizontal_top(|ui| {
                    ui.label(
                        RichText::new("▎")
                            .strong()
                            .color(ui.visuals().hyperlink_color),
                    );
                    ui.vertical(|ui| render_children(ui, children, list_depth, context));
                });
            });
        }
        Node::List { start, items } => {
            render_list(ui, *start, items, list_depth, context);
        }
        Node::Item {
            content, children, ..
        } => {
            if !content.is_empty() {
                render_inlines(ui, content, InlinePresentation::default(), context);
            }
            render_children(ui, children, list_depth + 1, context);
        }
        Node::CodeBlock { language, code } => {
            render_code_block(ui, language.as_deref(), code);
        }
        Node::FootnoteDefinition {
            label,
            content,
            children,
        } => {
            ui.horizontal_top(|ui| {
                ui.label(
                    RichText::new(format!("[{}]", context.footnote_number(label)))
                        .small()
                        .strong(),
                );
                ui.vertical(|ui| {
                    if !content.is_empty() {
                        render_inlines(ui, content, InlinePresentation::default(), context);
                    }
                    render_children(ui, children, list_depth, context);
                });
            });
        }
        Node::DefinitionList { children } => {
            render_children(ui, children, list_depth, context);
        }
        Node::DefinitionTerm { content } => {
            render_inlines(
                ui,
                content,
                InlinePresentation {
                    force_strong: true,
                    ..Default::default()
                },
                context,
            );
        }
        Node::DefinitionDescription { content, children } => {
            ui.horizontal_top(|ui| {
                ui.add_space(18.0);
                ui.label(RichText::new("—").weak());
                ui.vertical(|ui| {
                    if !content.is_empty() {
                        render_inlines(ui, content, InlinePresentation::default(), context);
                    }
                    render_children(ui, children, list_depth, context);
                });
            });
        }
        Node::Table {
            alignments,
            children,
        } => render_table(ui, alignments, children, list_depth, context),
        Node::TableHead { cells } | Node::TableRow { cells } => {
            render_children(ui, cells, list_depth, context);
        }
        Node::TableCell { content } => {
            render_inlines(ui, content, InlinePresentation::default(), context)
        }
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
    context: &mut RenderContext,
) {
    for (index, item) in items.iter().enumerate() {
        let marker = match item {
            Node::Item {
                task: Some(true), ..
            } => "☑ ".to_owned(),
            Node::Item {
                task: Some(false), ..
            } => "☐ ".to_owned(),
            _ => start
                .map(|first| format!("{}. ", first + index as u64))
                .unwrap_or_else(|| "• ".to_owned()),
        };
        ui.horizontal_top(|ui| {
            ui.add_space(list_depth as f32 * 14.0);
            ui.label(RichText::new(marker).monospace());
            ui.vertical(|ui| match item {
                Node::Item {
                    content, children, ..
                } => {
                    if !content.is_empty() {
                        render_inlines(ui, content, InlinePresentation::default(), context);
                    }
                    render_children(ui, children, list_depth + 1, context);
                }
                other => render_block(ui, other, list_depth + 1, context),
            });
        });
        if index + 1 < items.len() {
            ui.add_space(3.0);
        }
    }
}

fn render_table(
    ui: &mut Ui,
    alignments: &[MarkdownAlignment],
    children: &[Node],
    list_depth: usize,
    context: &mut RenderContext,
) {
    let id = ui.id().with(("markdown-table", context.next_table_id));
    context.next_table_id += 1;

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
                            for (column, cell) in cells.iter().enumerate() {
                                let alignment = alignments
                                    .get(column)
                                    .copied()
                                    .unwrap_or(MarkdownAlignment::None);
                                match cell {
                                    Node::TableCell { content } => {
                                        let align = match alignment {
                                            MarkdownAlignment::Center => Align::Center,
                                            MarkdownAlignment::Right => Align::RIGHT,
                                            MarkdownAlignment::None | MarkdownAlignment::Left => {
                                                Align::LEFT
                                            }
                                        };
                                        ui.allocate_ui_with_layout(
                                            Vec2::new(layout.column_width, 0.0),
                                            Layout::top_down(align),
                                            |ui| {
                                                render_inlines(
                                                    ui,
                                                    content,
                                                    InlinePresentation {
                                                        force_strong: header,
                                                        ..Default::default()
                                                    },
                                                    context,
                                                );
                                            },
                                        );
                                    }
                                    other => render_block(ui, other, list_depth, context),
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

fn render_inlines(
    ui: &mut Ui,
    content: &[Inline],
    presentation: InlinePresentation,
    context: &mut RenderContext,
) {
    let mut line_start = 0;
    for (index, inline) in content.iter().enumerate() {
        match &inline.kind {
            InlineKind::HardBreak => {
                render_inline_line(ui, &content[line_start..index], presentation, context);
                line_start = index + 1;
            }
            InlineKind::DisplayMath(formula) => {
                if line_start < index {
                    render_inline_line(ui, &content[line_start..index], presentation, context);
                }
                render_display_math(ui, formula);
                line_start = index + 1;
            }
            _ => {}
        }
    }
    if line_start < content.len() {
        render_inline_line(ui, &content[line_start..], presentation, context);
    } else if matches!(
        content.last().map(|inline| &inline.kind),
        Some(InlineKind::HardBreak)
    ) {
        ui.label(" ");
    }
}

fn render_inline_line(
    ui: &mut Ui,
    content: &[Inline],
    presentation: InlinePresentation,
    context: &RenderContext,
) {
    ui.horizontal_wrapped(|ui| {
        let original_spacing = ui.spacing().item_spacing;
        ui.spacing_mut().item_spacing.x = 0.0;
        for inline in content {
            match &inline.kind {
                InlineKind::Text(value) => {
                    render_rich_text(ui, value, &inline.style, presentation, false);
                }
                InlineKind::Code(value) => {
                    render_rich_text(ui, value, &inline.style, presentation, true);
                }
                InlineKind::InlineMath(formula) => {
                    let text = RichText::new(format_inline_math(formula))
                        .monospace()
                        .color(ui.visuals().strong_text_color());
                    ui.add(Label::new(text).wrap());
                }
                InlineKind::Image {
                    destination,
                    title,
                    alt,
                } => render_image_reference(ui, destination, title, alt),
                InlineKind::FootnoteReference(label) => {
                    ui.label(
                        RichText::new(format!("[{}]", context.footnote_number(label)))
                            .small()
                            .strong()
                            .color(ui.visuals().hyperlink_color),
                    );
                }
                InlineKind::HardBreak | InlineKind::DisplayMath(_) => {}
            }
        }
        ui.spacing_mut().item_spacing = original_spacing;
    });
}

fn render_rich_text(
    ui: &mut Ui,
    value: &str,
    style: &InlineStyle,
    presentation: InlinePresentation,
    code: bool,
) {
    let mut text = RichText::new(value);
    if code {
        text = text
            .monospace()
            .background_color(ui.visuals().code_bg_color);
    }
    if style.emphasis {
        text = text.italics();
    }
    if style.strong || presentation.force_strong {
        text = text.strong();
    }
    if style.strikethrough {
        text = text.strikethrough();
    }
    if style.underline {
        text = text.underline();
    }
    if let Some(font_size) = presentation.font_size {
        text = text.size(font_size);
    }

    if let Some(url) = style.link.as_deref() {
        ui.hyperlink_to(text, url);
    } else {
        ui.add(Label::new(text).wrap());
    }
}

fn render_image_reference(ui: &mut Ui, destination: &str, title: &str, alt: &str) {
    ui.group(|ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("▧").size(18.0).strong());
            let label = if alt.trim().is_empty() {
                "画像"
            } else {
                alt.trim()
            };
            ui.hyperlink_to(RichText::new(label).strong(), destination);
        });
        if !title.trim().is_empty() {
            ui.label(RichText::new(title.trim()).small().weak());
        }
        ui.label(RichText::new(destination).small().weak().monospace());
    });
}

fn render_code_block(ui: &mut Ui, language: Option<&str>, code: &str) {
    ui.group(|ui| {
        ui.take_available_width();
        if let Some(language) = language {
            ui.label(RichText::new(language).small().strong().monospace());
        }
        let job = highlighted_code_job(ui, language, code);
        ScrollArea::horizontal()
            .id_salt(ui.id().with(("markdown-code", code.as_ptr())))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.add(
                    Label::new(job)
                        .selectable(true)
                        .wrap_mode(TextWrapMode::Extend),
                );
            });
    });
}

fn highlighted_code_job(ui: &Ui, language: Option<&str>, code: &str) -> LayoutJob {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();
    let syntax_set = SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines);
    let theme_set = THEME_SET.get_or_init(ThemeSet::load_defaults);
    let theme_name = if ui.visuals().dark_mode {
        "base16-ocean.dark"
    } else {
        "InspiredGitHub"
    };
    let Some(theme) = theme_set
        .themes
        .get(theme_name)
        .or_else(|| theme_set.themes.values().next())
    else {
        return plain_code_job(ui, code);
    };

    let token = language.unwrap_or("text").trim().to_ascii_lowercase();
    let syntax = syntax_set
        .find_syntax_by_token(&token)
        .or_else(|| match token.as_str() {
            "js" => syntax_set.find_syntax_by_token("javascript"),
            "shell" | "sh" | "zsh" => syntax_set.find_syntax_by_token("bash"),
            "py" => syntax_set.find_syntax_by_token("python"),
            _ => None,
        })
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut job = LayoutJob::default();
    for line in LinesWithEndings::from(code) {
        let Ok(ranges) = highlighter.highlight_line(line, syntax_set) else {
            job.append(line, 0.0, code_text_format(ui.visuals().text_color()));
            continue;
        };
        append_highlighted_ranges(&mut job, &ranges);
    }
    job
}

fn append_highlighted_ranges(job: &mut LayoutJob, ranges: &[(syntect::highlighting::Style, &str)]) {
    for (style, text) in ranges {
        job.append(
            text,
            0.0,
            code_text_format(Color32::from_rgb(
                style.foreground.r,
                style.foreground.g,
                style.foreground.b,
            )),
        );
    }
}

fn plain_code_job(ui: &Ui, code: &str) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(code, 0.0, code_text_format(ui.visuals().text_color()));
    job
}

fn code_text_format(color: Color32) -> TextFormat {
    TextFormat {
        font_id: FontId::new(13.0, FontFamily::Monospace),
        color,
        ..Default::default()
    }
}

fn render_display_math(ui: &mut Ui, formula: &str) {
    ui.with_layout(Layout::top_down(Align::Center), |ui| {
        ui.group(|ui| {
            ui.label(
                RichText::new(format_display_math(formula))
                    .monospace()
                    .size(15.0)
                    .color(ui.visuals().strong_text_color()),
            );
        });
    });
}

fn format_inline_math(formula: &str) -> String {
    latex_to_unicode(formula.trim())
}

fn format_display_math(formula: &str) -> String {
    let formula = formula.trim();
    if let Some(matrix) = format_matrix(formula) {
        return matrix;
    }
    if let Some((prefix, numerator, denominator, suffix)) = first_fraction(formula) {
        let prefix = latex_to_unicode(prefix.trim_end());
        let numerator = latex_to_unicode(&numerator);
        let denominator = latex_to_unicode(&denominator);
        let suffix = latex_to_unicode(suffix.trim_start());
        let width = numerator
            .chars()
            .count()
            .max(denominator.chars().count())
            .max(3);
        let prefix_width = prefix.chars().count();
        let numerator_padding = width.saturating_sub(numerator.chars().count()) / 2;
        let denominator_padding = width.saturating_sub(denominator.chars().count()) / 2;
        let mut lines = vec![
            format!(
                "{}{}{}",
                " ".repeat(prefix_width + usize::from(!prefix.is_empty())),
                " ".repeat(numerator_padding),
                numerator
            ),
            format!(
                "{}{}{}",
                if prefix.is_empty() {
                    String::new()
                } else {
                    format!("{prefix} ")
                },
                "─".repeat(width),
                if suffix.is_empty() {
                    String::new()
                } else {
                    format!(" {suffix}")
                }
            ),
            format!(
                "{}{}{}",
                " ".repeat(prefix_width + usize::from(!prefix.is_empty())),
                " ".repeat(denominator_padding),
                denominator
            ),
        ];
        while lines.first().is_some_and(|line| line.trim().is_empty()) {
            lines.remove(0);
        }
        return lines.join("\n");
    }
    latex_to_unicode(formula)
}

fn first_fraction(formula: &str) -> Option<(&str, String, String, &str)> {
    let start = formula.find("\\frac")?;
    let mut cursor = start + "\\frac".len();
    let (numerator, next) = braced_argument(formula, cursor)?;
    cursor = next;
    let (denominator, next) = braced_argument(formula, cursor)?;
    Some((&formula[..start], numerator, denominator, &formula[next..]))
}

fn braced_argument(source: &str, mut index: usize) -> Option<(String, usize)> {
    while source[index..].starts_with(char::is_whitespace) {
        index += source[index..].chars().next()?.len_utf8();
    }
    if !source[index..].starts_with('{') {
        return None;
    }
    let content_start = index + 1;
    let mut depth = 1usize;
    index = content_start;
    while index < source.len() {
        let character = source[index..].chars().next()?;
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((source[content_start..index].to_owned(), index + 1));
                }
            }
            _ => {}
        }
        index += character.len_utf8();
    }
    None
}

fn format_matrix(formula: &str) -> Option<String> {
    const BEGIN: &str = "\\begin{pmatrix}";
    const END: &str = "\\end{pmatrix}";
    let begin = formula.find(BEGIN)?;
    let end = formula[begin + BEGIN.len()..].find(END)? + begin + BEGIN.len();
    let prefix = latex_to_unicode(formula[..begin].trim_end());
    let suffix = latex_to_unicode(formula[end + END.len()..].trim_start());
    let body = &formula[begin + BEGIN.len()..end];
    let rows: Vec<Vec<String>> = body
        .split("\\\\")
        .map(|row| {
            row.split('&')
                .map(|cell| latex_to_unicode(cell.trim()))
                .collect()
        })
        .filter(|row: &Vec<String>| row.iter().any(|cell| !cell.is_empty()))
        .collect();
    if rows.is_empty() {
        return None;
    }
    let columns = rows.iter().map(Vec::len).max().unwrap_or(1);
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let prefix_gap = " ".repeat(prefix.chars().count() + usize::from(!prefix.is_empty()));
    let mut rendered = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        let left = match (row_index, rows.len()) {
            (0, 1) => "(",
            (0, _) => "⎛",
            (index, len) if index + 1 == len => "⎝",
            _ => "⎜",
        };
        let right = match (row_index, rows.len()) {
            (0, 1) => ")",
            (0, _) => "⎞",
            (index, len) if index + 1 == len => "⎠",
            _ => "⎟",
        };
        let cells = (0..columns)
            .map(|column| {
                let value = row.get(column).map(String::as_str).unwrap_or_default();
                format!("{value:>width$}", width = widths[column])
            })
            .collect::<Vec<_>>()
            .join("  ");
        let leading = if row_index == 0 && !prefix.is_empty() {
            format!("{prefix} ")
        } else {
            prefix_gap.clone()
        };
        let trailing = if row_index == 0 && !suffix.is_empty() {
            format!(" {suffix}")
        } else {
            String::new()
        };
        rendered.push(format!("{leading}{left}{cells}{right}{trailing}"));
    }
    Some(rendered.join("\n"))
}

fn latex_to_unicode(source: &str) -> String {
    let mut rendered = String::with_capacity(source.len());
    let mut index = 0;
    while index < source.len() {
        let remaining = &source[index..];
        if remaining.starts_with("\\frac") {
            let cursor = index + "\\frac".len();
            if let Some((numerator, next)) = braced_argument(source, cursor)
                && let Some((denominator, end)) = braced_argument(source, next)
            {
                rendered.push('(');
                rendered.push_str(&latex_to_unicode(&numerator));
                rendered.push_str(")⁄(");
                rendered.push_str(&latex_to_unicode(&denominator));
                rendered.push(')');
                index = end;
                continue;
            }
        }
        if remaining.starts_with("\\sqrt") {
            let cursor = index + "\\sqrt".len();
            if let Some((radicand, end)) = braced_argument(source, cursor) {
                rendered.push_str("√(");
                rendered.push_str(&latex_to_unicode(&radicand));
                rendered.push(')');
                index = end;
                continue;
            }
        }
        if remaining.starts_with("\\begin{pmatrix}") {
            if let Some(matrix) = format_matrix(remaining) {
                rendered.push_str(&matrix);
                break;
            }
        }
        let commands = [
            ("\\infty", "∞"),
            ("\\sum", "∑"),
            ("\\int", "∫"),
            ("\\pm", "±"),
            ("\\pi", "π"),
            ("\\times", "×"),
            ("\\cdot", "·"),
            ("\\le", "≤"),
            ("\\ge", "≥"),
            ("\\ne", "≠"),
            ("\\,", " "),
        ];
        if let Some((command, replacement)) = commands
            .iter()
            .find(|(command, _)| remaining.starts_with(command))
        {
            rendered.push_str(replacement);
            index += command.len();
            continue;
        }
        if remaining.starts_with('^') || remaining.starts_with('_') {
            let superscript = remaining.starts_with('^');
            let cursor = index + 1;
            let (argument, end) = if source[cursor..].starts_with('{') {
                braced_argument(source, cursor).unwrap_or_else(|| (String::new(), cursor))
            } else if let Some(character) = source[cursor..].chars().next() {
                (character.to_string(), cursor + character.len_utf8())
            } else {
                (String::new(), cursor)
            };
            let argument = latex_to_unicode(&argument);
            rendered.push_str(&script_text(&argument, superscript));
            index = end;
            continue;
        }
        let character = remaining.chars().next().unwrap_or_default();
        if !matches!(character, '{' | '}') {
            rendered.push(character);
        }
        index += character.len_utf8();
    }
    rendered
}

fn script_text(source: &str, superscript: bool) -> String {
    source
        .chars()
        .map(|character| script_character(character, superscript).unwrap_or(character))
        .collect()
}

fn script_character(character: char, superscript: bool) -> Option<char> {
    let normal = "0123456789+-=()abdeghijklmnoprstuvx";
    let supers = "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻⁼⁽⁾ᵃᵇᵈᵉᵍʰⁱʲᵏˡᵐⁿᵒᵖʳˢᵗᵘᵛˣ";
    let subs = "₀₁₂₃₄₅₆₇₈₉₊₋₌₍₎ₐᵦ_dₑ_gₕᵢⱼₖₗₘₙₒₚᵣₛₜᵤᵥₓ";
    let index = normal
        .chars()
        .position(|candidate| candidate == character)?;
    if superscript {
        supers.chars().nth(index)
    } else {
        subs.chars().nth(index).filter(|mapped| *mapped != '_')
    }
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
        HeadingLevel::H5 => 1.0,
        HeadingLevel::H6 => 0.9,
    };
    body_size * multiplier
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKDOWN_ACCEPTANCE_FIXTURE: &str = r#"# 見出し H1
## 見出し H2
### 見出し H3
#### 見出し H4
##### 見出し H5
###### 見出し H6

これは**太字**、__別の太字__、*斜体*、_別の斜体_、***太字＋斜体***、~~取り消し~~、**太字の中に _斜体_**、***~~全部~~***です。

文章中に `printf("Hello");` と `git status` を入れます。

~~~text
plain
    indent
~~~
~~~c
#include <stdio.h>
~~~
~~~python
def hello(name):
    return f"Hello, {name}!"
~~~
~~~javascript
const enabled = true;
~~~
~~~json
{"enabled": true, "count": 42}
~~~
~~~bash
git status
~~~

> ### システム状態
>
> **現在の状態：正常**
>
> - Web Server
>   - Port：`443`
>
> > 二重引用
> >
> > > 三重引用
>
> ~~~bash
> systemctl status nginx
> ~~~

- コンピューター
  - ハードウェア
    - CPU
    - RAM
  - ソフトウェア
1. 最初
2. 次
   1. サブ項目
   2. サブ項目

- [x] Markdownを書く
- [ ] 世界征服

[OpenAI](https://openai.com/)
<https://example.com/>

![代替テキスト](https://example.com/image.png "画像タイトル")

---
***
___

| 左寄せ | 中央寄せ | 右寄せ |
|:---|:---:|---:|
| **Apple** | `Mac` | ~~100~~ |

\*これは斜体にならない\*
\# これは見出しにならない

同じ段落です。\
ここは明示改行です。

<strong>HTMLの太字</strong>
<em>HTMLの斜体</em>
<del>HTMLの削除</del>
<u>HTMLの下線</u>

インライン数式：\( E = mc^2 \)

\[
x = \frac{-b \pm \sqrt{b^2 - 4ac}}{2a}
\]

\[
\sum_{i=1}^{n} i = \frac{n(n+1)}{2}
\]

\[
\int_{-\infty}^{\infty} e^{-x^2}\,dx = \sqrt{\pi}
\]

\[
A =
\begin{pmatrix}
1 & 2 \\
3 & 4
\end{pmatrix}
\]

これは脚注付き文章です。[^1] 別の脚注です。[^note]

[^1]: これが脚注の内容です。
[^note]: Markdown処理系ごとの差です。

Markdown
: 軽量マークアップ言語

HTML
: HyperText Markup Language
"#;

    #[derive(Default)]
    struct FeatureSummary {
        headings: Vec<HeadingLevel>,
        code_languages: Vec<Option<String>>,
        block_quotes: usize,
        lists: usize,
        tasks: Vec<bool>,
        images: usize,
        links: usize,
        rules: usize,
        table_alignments: Vec<MarkdownAlignment>,
        hard_breaks: usize,
        inline_math: usize,
        display_math: usize,
        footnote_references: usize,
        footnote_definitions: usize,
        definition_lists: usize,
        strong: usize,
        emphasis: usize,
        strikethrough: usize,
        underline: usize,
        raw_safe_html_tags: usize,
    }

    fn summarize(nodes: &[Node], summary: &mut FeatureSummary) {
        for node in nodes {
            match node {
                Node::Heading { level, .. } => summary.headings.push(*level),
                Node::CodeBlock { language, .. } => {
                    summary.code_languages.push(language.clone());
                }
                Node::BlockQuote { .. } => summary.block_quotes += 1,
                Node::List { .. } => summary.lists += 1,
                Node::Item {
                    task: Some(checked),
                    ..
                } => summary.tasks.push(*checked),
                Node::FootnoteDefinition { .. } => summary.footnote_definitions += 1,
                Node::DefinitionList { .. } => summary.definition_lists += 1,
                Node::Rule => summary.rules += 1,
                Node::Table { alignments, .. } => {
                    summary.table_alignments = alignments.clone();
                }
                _ => {}
            }
            for inline in node_inlines(node) {
                summary.strong += usize::from(inline.style.strong);
                summary.emphasis += usize::from(inline.style.emphasis);
                summary.strikethrough += usize::from(inline.style.strikethrough);
                summary.underline += usize::from(inline.style.underline);
                match &inline.kind {
                    InlineKind::Image { .. } => summary.images += 1,
                    InlineKind::HardBreak => summary.hard_breaks += 1,
                    InlineKind::InlineMath(_) => summary.inline_math += 1,
                    InlineKind::DisplayMath(_) => summary.display_math += 1,
                    InlineKind::FootnoteReference(_) => summary.footnote_references += 1,
                    InlineKind::Text(text) => {
                        summary.raw_safe_html_tags += ["<strong>", "<em>", "<del>", "<u>"]
                            .iter()
                            .filter(|tag| text.contains(**tag))
                            .count();
                    }
                    _ => {}
                }
                summary.links += usize::from(inline.style.link.is_some());
            }
            summarize(node_children(node), summary);
        }
    }

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
        let Some(Node::Table { children, .. }) = children.first() else {
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

    #[test]
    fn parses_the_complete_markdown_acceptance_fixture() {
        let document = parse(MARKDOWN_ACCEPTANCE_FIXTURE);
        let Node::Root { children } = &document else {
            panic!("root document expected");
        };
        let mut summary = FeatureSummary::default();
        summarize(children, &mut summary);

        assert_eq!(
            summary.headings[..6],
            [
                HeadingLevel::H1,
                HeadingLevel::H2,
                HeadingLevel::H3,
                HeadingLevel::H4,
                HeadingLevel::H5,
                HeadingLevel::H6,
            ]
        );
        for language in ["text", "c", "python", "javascript", "json", "bash"] {
            assert!(
                summary
                    .code_languages
                    .iter()
                    .any(|candidate| candidate.as_deref() == Some(language)),
                "missing {language} code block"
            );
        }
        assert!(summary.block_quotes >= 3);
        assert!(summary.lists >= 4);
        assert_eq!(summary.tasks, [true, false]);
        assert_eq!(summary.images, 1);
        assert!(summary.links >= 2);
        assert_eq!(summary.rules, 3);
        assert_eq!(
            summary.table_alignments,
            [
                MarkdownAlignment::Left,
                MarkdownAlignment::Center,
                MarkdownAlignment::Right,
            ]
        );
        assert!(summary.hard_breaks >= 1);
        assert_eq!(summary.inline_math, 1);
        assert_eq!(summary.display_math, 4);
        assert_eq!(summary.footnote_references, 2);
        assert_eq!(summary.footnote_definitions, 2);
        assert_eq!(summary.definition_lists, 1);
        assert!(summary.strong >= 5);
        assert!(summary.emphasis >= 4);
        assert!(summary.strikethrough >= 3);
        assert!(summary.underline >= 1);
        assert_eq!(summary.raw_safe_html_tags, 0);
    }

    #[test]
    fn draws_the_complete_markdown_acceptance_fixture_without_panicking() {
        eframe::egui::__run_test_ui(|ui| show(ui, MARKDOWN_ACCEPTANCE_FIXTURE));
    }

    #[test]
    fn math_delimiter_normalization_leaves_fenced_and_inline_code_unchanged() {
        let markdown = "\\(x^2\\)\n\n`\\(code\\)`\n\n~~~text\n\\[code\\]\n~~~\n";
        let normalized = normalize_math_delimiters(markdown);
        assert!(normalized.starts_with("$x^2$"));
        assert!(normalized.contains("`\\(code\\)`"));
        assert!(normalized.contains("~~~text\n\\[code\\]\n~~~"));
    }

    #[test]
    fn two_space_hard_break_is_preserved() {
        let markdown = ["同じ段落です。", "  ", "\n次の行です。"].concat();
        let document = parse(&markdown);
        let Node::Root { children } = &document else {
            panic!("root document expected");
        };
        let mut summary = FeatureSummary::default();
        summarize(children, &mut summary);
        assert_eq!(summary.hard_breaks, 1);
    }

    #[test]
    fn formats_the_sample_fraction_and_matrix_as_native_math_text() {
        let fraction = format_display_math("x = \\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}");
        assert!(fraction.contains('±'));
        assert!(fraction.contains('√'));
        assert!(fraction.contains('─'));
        assert!(!fraction.contains("\\frac"));

        let matrix = format_display_math("A = \\begin{pmatrix}1 & 2 \\\\ 3 & 4\\end{pmatrix}");
        assert!(matrix.contains('⎛'));
        assert!(matrix.contains('⎝'));
        assert!(!matrix.contains("pmatrix"));
    }
}
