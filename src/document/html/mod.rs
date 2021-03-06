pub mod dom;
pub mod xml;
pub mod css;
pub mod parse;
pub mod style;
pub mod layout;
pub mod engine;

use std::io::Read;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use fxhash::FxHashMap;
use anyhow::Error;
use crate::framebuffer::Pixmap;
use crate::helpers::{Normalize, decode_entities};
use crate::document::{Document, Location, TextLocation, TocEntry, BoundedText};
use crate::unit::pt_to_px;
use crate::geom::{Rectangle, Edge, CycleDir};
use self::dom::Node;
use self::layout::{RootData, StyleData, DrawState, LoopContext};
use self::layout::{DrawCommand, TextCommand, ImageCommand, TextAlign};
use self::engine::{Page, Engine, ResourceFetcher};
use self::css::{CssParser, RuleKind};
use self::xml::XmlParser;

const VIEWER_STYLESHEET: &str = "css/html.css";
const USER_STYLESHEET: &str = "css/html-user.css";

type UriCache = FxHashMap<String, usize>;

pub struct HtmlDocument {
    content: Node,
    engine: Engine,
    pages: Vec<Page>,
    parent: PathBuf,
    size: usize,
    viewer_stylesheet: PathBuf,
    user_stylesheet: PathBuf,
    ignore_document_css: bool,
}

impl ResourceFetcher for PathBuf {
    fn fetch(&mut self, name: &str) -> Result<Vec<u8>, Error> {
        let mut file = File::open(self.join(name))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        Ok(buf)
    }
}

unsafe impl Send for HtmlDocument {}
unsafe impl Sync for HtmlDocument {}

impl HtmlDocument {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<HtmlDocument, Error> {
        let mut file = File::open(&path)?;
        let size = file.metadata()?.len() as usize;
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        let mut content = XmlParser::new(&content).parse();
        content.wrap_lost_inlines();
        let parent = path.as_ref().parent().unwrap_or_else(|| Path::new(""));

        Ok(HtmlDocument {
            content,
            engine: Engine::new(),
            pages: Vec::new(),
            parent: parent.to_path_buf(),
            size,
            viewer_stylesheet: PathBuf::from(VIEWER_STYLESHEET),
            user_stylesheet: PathBuf::from(USER_STYLESHEET),
            ignore_document_css: false,
        })
    }

    pub fn new_from_memory(content: &str) -> HtmlDocument {
        let size = content.len();
        let mut content = XmlParser::new(content).parse();
        content.wrap_lost_inlines();

        HtmlDocument {
            content,
            engine: Engine::new(),
            pages: Vec::new(),
            parent: PathBuf::from(""),
            size,
            viewer_stylesheet: PathBuf::from(VIEWER_STYLESHEET),
            user_stylesheet: PathBuf::from(USER_STYLESHEET),
            ignore_document_css: false,
        }
    }

    pub fn update(&mut self, content: &str) {
        self.size = content.len();
        self.content = XmlParser::new(content).parse();
        self.content.wrap_lost_inlines();
        self.pages.clear();
    }

    pub fn set_margin(&mut self, margin: &Edge) {
        self.engine.set_margin(margin);
        self.pages.clear();
    }

    pub fn set_font_size(&mut self, font_size: f32) {
        self.engine.set_font_size(font_size);
        self.pages.clear();
    }

    pub fn set_viewer_stylesheet<P: AsRef<Path>>(&mut self, path: P) {
        self.viewer_stylesheet = path.as_ref().to_path_buf();
        self.pages.clear();
    }

    pub fn set_user_stylesheet<P: AsRef<Path>>(&mut self, path: P) {
        self.user_stylesheet = path.as_ref().to_path_buf();
        self.pages.clear();
    }

    #[inline]
    fn page_index(&mut self, offset: usize) -> Option<usize> {
        if self.pages.is_empty() {
            self.pages = self.build_pages();
        }
        if self.pages.len() < 2 || self.pages[1].first().map(|dc| offset < dc.offset()) == Some(true) {
            return Some(0);
        } else if self.pages[self.pages.len() - 1].first().map(|dc| offset >= dc.offset()) == Some(true) {
            return Some(self.pages.len() - 1);
        } else {
            for i in 1..self.pages.len()-1 {
                if self.pages[i].first().map(|dc| offset >= dc.offset()) == Some(true) &&
                   self.pages[i+1].first().map(|dc| offset < dc.offset()) == Some(true) {
                    return Some(i);
                }
            }
        }
        None
    }

    fn resolve_link(&mut self, uri: &str, cache: &mut UriCache) -> Option<usize> {
        let frag_index = uri.find('#')?;
        let name = &uri[..frag_index];
        let content = self.content.clone();
        self.cache_uris(&content, name, cache);
        cache.get(uri).cloned()
    }

    fn cache_uris(&mut self, node: &Node, name: &str, cache: &mut UriCache) {
        if let Some(id) = node.attr("id") {
            cache.insert(format!("{}#{}", name, id), node.offset());
        }
        if let Some(children) = node.children() {
            for child in children {
                self.cache_uris(child, name, cache);
            }
        }
    }

    fn images(&mut self, loc: Location) -> Option<(Vec<Rectangle>, usize)> {
        let offset = self.resolve_location(loc)?;
        let page_index = self.page_index(offset)?;

        Some((self.pages[page_index].iter().filter_map(|dc| {
            match dc {
                DrawCommand::Image(ImageCommand { rect, .. }) => Some(*rect),
                _ => None,
            }
        }).collect(), offset))
    }

    fn build_pages(&mut self) -> Vec<Page> {
        let mut stylesheet = Vec::new();
        let spine_dir = PathBuf::from("");

        if let Ok(text) = fs::read_to_string(&self.viewer_stylesheet) {
            let (mut css, _) = CssParser::new(&text).parse(RuleKind::Viewer);
            stylesheet.append(&mut css);
        }

        if let Ok(text) = fs::read_to_string(&self.user_stylesheet) {
            let (mut css, _) = CssParser::new(&text).parse(RuleKind::User);
            stylesheet.append(&mut css);
        }

        if !self.ignore_document_css {
            if let Some(head) = self.content.find("head") {
                if let Some(children) = head.children() {
                    for child in children {
                        if child.tag_name() == Some("link") && child.attr("rel") == Some("stylesheet") {
                            if let Some(href) = child.attr("href") {
                                if let Some(name) = spine_dir.join(href).normalize().to_str() {
                                    if let Ok(buf) = self.parent.fetch(name) {
                                        if let Ok(text) = String::from_utf8(buf) {
                                            let (mut css, _) = CssParser::new(&text).parse(RuleKind::Document);
                                            stylesheet.append(&mut css);
                                        }
                                    }
                                }
                            }
                        } else if child.tag_name() == Some("style") && child.attr("type") == Some("text/css") {
                            if let Some(text) = child.text() {
                                let (mut css, _) = CssParser::new(text).parse(RuleKind::Document);
                                stylesheet.append(&mut css);
                            }
                        }
                    }
                }
            }
        }

        let mut pages = Vec::new();

        let mut rect = self.engine.rect();
        rect.shrink(&self.engine.margin);

        let language = self.content.find("html")
                           .and_then(|html| html.attr("xml:lang"))
                           .map(String::from);

        let style = StyleData {
            language,
            font_size: self.engine.font_size,
            line_height: pt_to_px(self.engine.line_height * self.engine.font_size, self.engine.dpi).round() as i32,
            text_align: self.engine.text_align,
            start_x: rect.min.x,
            end_x: rect.max.x,
            width: rect.max.x - rect.min.x,
            .. Default::default()
        };

        let loop_context = LoopContext::default();
        let mut draw_state = DrawState {
            position: rect.min,
            .. Default::default()
        };

        let root_data = RootData {
            start_offset: 0,
            spine_dir,
            rect,
        };

        pages.push(Vec::new());

        self.engine.build_display_list(&self.content, &style, &loop_context, &stylesheet, &root_data, &mut self.parent, &mut draw_state, &mut pages);

        pages.retain(|page| !page.is_empty());

        if pages.is_empty() {
            pages.push(vec![DrawCommand::Marker(self.content.offset())]);
        }

        pages
    }

    pub fn categories(&self) -> Option<String> {
        None
    }

    pub fn description(&self) -> Option<String> {
        self.metadata("description")
    }

    pub fn language(&self) -> Option<String> {
        self.content.find("html")
            .and_then(|html| html.attr("xml:lang"))
            .map(String::from)
    }

    pub fn year(&self) -> Option<String> {
        self.metadata("date").map(|s| s.chars().take(4).collect())
    }
}

impl Document for HtmlDocument {
    #[inline]
    fn dims(&self, _index: usize) -> Option<(f32, f32)> {
        Some((self.engine.dims.0 as f32, self.engine.dims.1 as f32))
    }

    fn pages_count(&self) -> usize {
        self.size
    }

    fn toc(&mut self) -> Option<Vec<TocEntry>> {
        None
    }

    fn chapter<'a>(&mut self, _offset: usize, _toc: &'a [TocEntry]) -> Option<&'a TocEntry> {
        None
    }

    fn chapter_relative<'a>(&mut self, _offset: usize, _dir: CycleDir, _toc: &'a [TocEntry]) -> Option<&'a TocEntry> {
        None
    }

    fn resolve_location(&mut self, loc: Location) -> Option<usize> {
        self.engine.load_fonts();

        match loc {
            Location::Exact(offset) => {
                let page_index = self.page_index(offset)?;
                self.pages[page_index].first()
                    .map(DrawCommand::offset)
            },
            Location::Previous(offset) => {
                let page_index = self.page_index(offset)?;
                if page_index > 0 {
                    self.pages[page_index-1].first().map(DrawCommand::offset)
                } else {
                    None
                }
            },
            Location::Next(offset) => {
                let page_index = self.page_index(offset)?;
                if page_index < self.pages.len() - 1 {
                    self.pages[page_index+1].first().map(DrawCommand::offset)
                } else {
                    None
                }
            },
            Location::LocalUri(_, ref uri) | Location::Uri(ref  uri) => {
                let mut cache = FxHashMap::default();
                self.resolve_link(uri, &mut cache)
            },
        }
    }

    fn words(&mut self, loc: Location) -> Option<(Vec<BoundedText>, usize)> {
        let offset = self.resolve_location(loc)?;
        let page_index = self.page_index(offset)?;

        Some((self.pages[page_index].iter().filter_map(|dc| {
            match dc {
                DrawCommand::Text(TextCommand { text, rect, offset, .. }) => {
                    Some(BoundedText {
                        text: text.clone(),
                        rect: (*rect).into(),
                        location: TextLocation::Dynamic(*offset),
                    })
                },
                _ => None,
            }
        }).collect(), offset))
    }

    fn lines(&mut self, _loc: Location) -> Option<(Vec<BoundedText>, usize)> {
        None
    }

    fn links(&mut self, loc: Location) -> Option<(Vec<BoundedText>, usize)> {
        let offset = self.resolve_location(loc)?;
        let page_index = self.page_index(offset)?;

        Some((self.pages[page_index].iter().filter_map(|dc| {
            match dc {
                DrawCommand::Text(TextCommand { uri, rect, offset, .. }) |
                DrawCommand::Image(ImageCommand { uri, rect, offset, .. }) if uri.is_some() => {
                    Some(BoundedText {
                        text: uri.clone().unwrap(),
                        rect: (*rect).into(),
                        location: TextLocation::Dynamic(*offset),
                    })
                },
                _ => None,
            }
        }).collect(), offset))
    }

    fn pixmap(&mut self, loc: Location, _scale: f32) -> Option<(Pixmap, usize)> {
        let offset = self.resolve_location(loc)?;
        let page_index = self.page_index(offset)?;
        let page = self.pages[page_index].clone();
        let pixmap = self.engine.render_page(&page, &mut self.parent);

        Some((pixmap, offset))
    }

    fn layout(&mut self, width: u32, height: u32, font_size: f32, dpi: u16) {
        self.engine.layout(width, height, font_size, dpi);
        self.pages.clear();
    }

    fn set_text_align(&mut self, text_align: TextAlign) {
        self.engine.set_text_align(text_align);
        self.pages.clear();
    }

    fn set_font_family(&mut self, family_name: &str, search_path: &str) {
        self.engine.set_font_family(family_name, search_path);
        self.pages.clear();
    }

    fn set_margin_width(&mut self, width: i32) {
        self.engine.set_margin_width(width);
        self.pages.clear();
    }

    fn set_line_height(&mut self, line_height: f32) {
        self.engine.set_line_height(line_height);
        self.pages.clear();
    }

    fn title(&self) -> Option<String> {
        self.content.find("head")
            .and_then(Node::children)
            .and_then(|children| children.iter().find(|child| child.tag_name() == Some("title")))
            .and_then(|child| child.children().and_then(|c| c.get(0)))
            .and_then(|child| child.text().map(|s| decode_entities(s).into_owned()))
    }

    fn author(&self) -> Option<String> {
        self.metadata("author")
    }

    fn metadata(&self, key: &str) -> Option<String> {
        self.content.find("head")
            .and_then(Node::children)
            .and_then(|children| children.iter().find(|child| child.tag_name() == Some("meta") && child.attr("name") == Some(key)))
            .and_then(|child| child.attr("content").map(|s| decode_entities(s).into_owned()))
    }

    fn is_reflowable(&self) -> bool {
        true
    }

    fn has_synthetic_page_numbers(&self) -> bool {
        true
    }
}
