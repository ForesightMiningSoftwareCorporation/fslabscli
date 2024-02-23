use std::io::prelude::*;
use std::{collections::HashMap, fs::File, path::PathBuf};

pub struct SummaryTableRow {
    pub header: bool,
    pub data: String,
    pub colspan: Option<usize>,
    pub rowspan: Option<usize>,
}

impl SummaryTableRow {
    pub fn new(data: String) -> Self {
        Self {
            header: false,
            data,
            colspan: None,
            rowspan: None,
        }
    }
    pub fn new_header(data: String) -> Self {
        Self {
            header: true,
            data,
            colspan: None,
            rowspan: None,
        }
    }
}

pub struct Summary {
    pub buffer: String,
    pub file_path: PathBuf,
}

impl Summary {
    pub fn new(file_path: PathBuf) -> Self {
        Self {
            buffer: String::new(),
            file_path,
        }
    }

    fn wrap(
        &self,
        tag: String,
        content: Option<String>,
        attrs: Option<HashMap<String, String>>,
        inline: bool,
    ) -> String {
        let htmlAttrs: String = match attrs {
            Some(a) => a
                .into_iter()
                .map(|(k, v)| {
                    if v.is_empty() {
                        return format!(" {}", k);
                    }
                    format!(" {}={}", k, v)
                })
                .collect::<Vec<String>>()
                .join(""),
            None => "".to_string(),
        };
        let new_line = match inline {
            true => "",
            false => "\n",
        };
        match content {
            Some(c) => format!("<{tag}{htmlAttrs}>{new_line}{c}{new_line}</{tag}>"),
            None => format!("<{tag}{htmlAttrs}>"),
        }
    }

    pub async fn write(&self, overwrite: bool) -> anyhow::Result<()> {
        let mut file = File::create(self.file_path.as_path())?;
        file.write_all(self.buffer.as_bytes())?;
        Ok(())
    }

    pub async fn clear(&mut self) -> anyhow::Result<()> {
        self.buffer = "".to_string();
        self.write(true).await
    }

    pub fn add_content(&mut self, content: String, addEOL: bool) {
        self.buffer += &content;
        if addEOL {
            self.buffer += "\n";
        }
    }

    pub fn code_block(&self, code: String, lang: Option<String>) -> String {
        let attrs = lang.map(|l| HashMap::from([("lang".to_string(), l)]));
        self.wrap(
            "pre".to_string(),
            Some(self.wrap("code".to_string(), Some(code), None, true)),
            attrs,
            true,
        )
    }

    pub fn list(&mut self, items: Vec<String>, ordered: bool) -> String {
        let tag = match ordered {
            true => "ol".to_string(),
            false => "ul".to_string(),
        };
        let list_items = items
            .iter()
            .map(|i| self.wrap("li".to_string(), Some(i.clone()), None, false))
            .collect();
        self.wrap(tag, Some(list_items), None, true)
    }

    pub fn table(&self, rows: Vec<Vec<SummaryTableRow>>) -> String {
        let table_body = rows
            .iter()
            .map(|row| {
                let row_string = row
                    .iter()
                    .map(|cell| {
                        let tag = match cell.header {
                            true => "th".to_string(),
                            false => "td".to_string(),
                        };
                        let attrs = HashMap::from([
                            (
                                "colspan".to_string(),
                                format!("{}", cell.colspan.unwrap_or(1)),
                            ),
                            (
                                "rowspan".to_string(),
                                format!("{}", cell.rowspan.unwrap_or(1)),
                            ),
                        ]);
                        self.wrap(tag, Some(cell.data.to_string()), Some(attrs), false)
                    })
                    .collect::<Vec<String>>()
                    .join("\n");
                self.wrap("tr".to_string(), Some(row_string), None, false)
            })
            .collect::<Vec<String>>()
            .join("\n");
        self.wrap("table".to_string(), Some(table_body), None, true)
    }

    pub fn detail(&self, label: String, content: String, open: bool) -> String {
        let attrs = match open {
            true => Some(HashMap::from([("open".to_string(), "".to_string())])),
            false => None,
        };
        self.wrap(
            "details".to_string(),
            Some(format!(
                "{}\n{}",
                self.wrap("summary".to_string(), Some(label), None, true),
                content
            )),
            attrs,
            false,
        )
    }

    pub fn image(
        &self,
        src: String,
        alt: String,
        width: Option<String>,
        height: Option<String>,
    ) -> String {
        let mut attrs = HashMap::from([("src".to_string(), src), ("alt".to_string(), alt)]);
        if let Some(width) = width {
            attrs.insert("width".to_string(), width);
        }
        if let Some(height) = height {
            attrs.insert("height".to_string(), height);
        }
        self.wrap("img".to_string(), None, Some(attrs), false)
    }

    pub fn heading(&self, text: String, level: Option<usize>) -> String {
        let tag = format!("h{}", level.unwrap_or(1));
        self.wrap(tag, Some(text), None, false)
    }

    pub fn separator(&self) -> String {
        self.wrap("hr".to_string(), None, None, false)
    }

    pub fn line_break(&self) -> String {
        self.wrap("hr".to_string(), None, None, false)
    }

    pub fn quote(&self, text: String, cite: Option<String>) -> String {
        let attrs = cite.map(|c| HashMap::from([("cite".to_string(), c)]));
        self.wrap("blockquote".to_string(), Some(text), attrs, false)
    }

    pub fn link(&self, text: String, href: String) -> String {
        self.wrap(
            "a".to_string(),
            Some(text),
            Some(HashMap::from([("href".to_string(), href)])),
            false,
        )
    }

    pub fn p(&self, text: String) -> String {
        self.wrap("p".to_string(), Some(text), None, false)
    }
}
