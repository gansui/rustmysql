use mysql::*;
use mysql::prelude::*;
use rustyline::Editor;
use rustyline::error::ReadlineError;
use rustyline::completion::{Completer, Pair};
use rustyline::config::Configurer;
use rustyline::Context;
use std::borrow::Cow;
use structopt::StructOpt;
use prettytable::{Table, Row as PrettyRow, Cell, format};
use std::error::Error;
use std::path::PathBuf;
use std::fs::{OpenOptions, remove_file};
use std::io::{Write, BufRead, BufReader};
use std::time::{SystemTime, UNIX_EPOCH};
use dirs::home_dir;
use colored::*;
use regex::Regex;
use unicode_width::UnicodeWidthStr;
use fd_lock::RwLock;

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn truncate_str(s: &str, max_width: usize) -> String {
    let w = display_width(s);
    if w <= max_width {
        s.to_string()
    } else if max_width <= 0 {
        String::new()
    } else {
        let mut width = 0;
        let mut end = 0;
        for (i, ch) in s.char_indices() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if width + cw > max_width - 1 {
                break;
            }
            width += cw;
            end = i + ch.len_utf8();
        }
        format!("{}…", &s[..end])
    }
}

fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![];
    }
    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;

    for ch in text.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + cw > max_width && !current_line.is_empty() {
            lines.push(std::mem::take(&mut current_line));
            current_width = 0;
        }
        current_line.push(ch);
        current_width += cw;
    }
    if !current_line.is_empty() {
        lines.push(current_line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[derive(Clone, Copy, PartialEq)]
enum DisplayMode {
    Vertical,
    Mline(usize),
    Raw,
}

#[derive(Clone)]
struct SqlCompleter {
    databases: Vec<String>,
    tables: Vec<String>,
    columns: Vec<String>,
    keywords: Vec<String>,
}

impl SqlCompleter {
    fn new(conn: &mut Conn, current_db: &Option<String>) -> Self {
        let databases: Vec<String> = conn.query("SHOW DATABASES").unwrap_or_default();

        let tables: Vec<String> = if let Some(db) = current_db {
            let q = format!("SHOW TABLES FROM `{}`", db);
            conn.query(q).unwrap_or_default()
        } else {
            Vec::new()
        };

        let mut columns = Vec::new();
        if let Some(db) = current_db {
            for table in &tables {
                let q = format!("SHOW COLUMNS FROM `{}`.`{}`", db, table);
                if let Ok(rows) = conn.query::<mysql::Row, _>(q) {
                    for row in rows {
                        if let Some(name) = row.get::<String, usize>(0) {
                            if !columns.contains(&name) {
                                columns.push(name);
                            }
                        }
                    }
                }
            }
        }

        let keywords: Vec<String> = SQL_KEYWORDS.iter().map(|s| s.to_string()).collect();

        SqlCompleter { databases, tables, columns, keywords }
    }

    fn refresh(&mut self, conn: &mut Conn, current_db: &Option<String>) {
        self.databases = conn.query("SHOW DATABASES").unwrap_or_default();

        self.tables = if let Some(db) = current_db {
            let q = format!("SHOW TABLES FROM `{}`", db);
            conn.query(q).unwrap_or_default()
        } else {
            Vec::new()
        };

        self.columns.clear();
        if let Some(db) = current_db {
            for table in &self.tables.clone() {
                let q = format!("SHOW COLUMNS FROM `{}`.`{}`", db, table);
                if let Ok(rows) = conn.query::<mysql::Row, _>(q) {
                    for row in rows {
                        if let Some(name) = row.get::<String, usize>(0) {
                            if !self.columns.contains(&name) {
                                self.columns.push(name);
                            }
                        }
                    }
                }
            }
        }
    }
}

impl Completer for SqlCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> Result<(usize, Vec<Pair>), ReadlineError> {
        let tokens: Vec<&str> = line[..pos].split_whitespace().collect();
        let last_token = tokens.last().copied().unwrap_or("");
        let start = pos - last_token.len();

        let last_upper = last_token.to_uppercase();
        let prev_upper = tokens.len().checked_sub(2)
            .and_then(|i| tokens.get(i))
            .map(|s| s.to_uppercase())
            .unwrap_or_default();

        let candidates: Vec<String> = if prev_upper == "USE" {
            self.databases.iter()
                .filter(|d| d.to_uppercase().starts_with(&last_upper))
                .cloned().collect()
        } else if prev_upper == "FROM" || prev_upper == "JOIN" || prev_upper == "INTO"
            || prev_upper == "UPDATE" || prev_upper == "TABLE" || prev_upper == "DELETE" {
            self.tables.iter()
                .filter(|t| t.to_uppercase().starts_with(&last_upper))
                .cloned().collect()
        } else if prev_upper == "SELECT" || prev_upper == "WHERE" || prev_upper == "AND"
            || prev_upper == "OR" || prev_upper == "ON" || prev_upper == "HAVING"
            || prev_upper == "SET" || prev_upper == "ORDER" || prev_upper == "GROUP" || prev_upper == "BY" {
            let opts: Vec<String> = if prev_upper == "SELECT" {
                let mut v = vec!["*".to_string()];
                v.extend(self.columns.iter().filter(|c| c.to_uppercase().starts_with(&last_upper)).cloned());
                v
            } else {
                self.columns.iter()
                    .filter(|c| c.to_uppercase().starts_with(&last_upper))
                    .cloned().collect()
            };
            opts
        } else if last_upper.is_empty() || !last_upper.chars().next().map_or(false, |c| c.is_alphabetic()) {
            Vec::new()
        } else {
            let mut candidates: Vec<String> = BUILDIN_COMMAND.iter()
                .filter(|k| k.to_uppercase().starts_with(&last_upper) && k.to_uppercase() != last_upper)
                .map(|k| k.to_string())
                .collect();
            let kw_candidates: Vec<String> = self.keywords.iter()
                .filter(|k| k.to_uppercase().starts_with(&last_upper) && k.to_uppercase() != last_upper)
                .map(|k| k.to_lowercase())
                .collect();
            candidates.extend(kw_candidates);
            candidates
        };

        let pairs: Vec<Pair> = candidates.into_iter().map(|s| Pair {
            display: s.clone(),
            replacement: format!("{} ", s),
        }).collect();

        Ok((start, pairs))
    }
}

#[derive(StructOpt, Debug)]
#[structopt(name = "mysql", about = "Cross-platform MySQL client",
            setting = structopt::clap::AppSettings::DisableHelpFlags)]
struct Opts {
    #[structopt(short = "h", long, default_value = "localhost")]
    host: String,

    #[structopt(short = "H", long = "help")]
    help: bool,

    #[structopt(short = "P", long, default_value = "3306")]
    port: u16,

    #[structopt(short = "u", long)]
    user: Option<String>,

    #[structopt(short = "p", long)]
    password: Option<String>,

    #[structopt(short = "D", long)]
    database: Option<String>,

    #[structopt(short = "e", long)]
    execute: Option<String>,

    #[structopt(long)]
    no_color_output: bool,

    #[structopt(long)]
    no_vertical_output: bool,
}

struct MySQLClient {
    conn: Conn,
    current_db: Option<String>,
    use_colors: bool,
    host: String,
    port: u16,
    display_mode: DisplayMode,
    color_output: bool,
    highlight_color: String,
    oneline_mode: bool,
    sql_keyword_re: Regex,
    pager_command: Option<String>,
    completer: SqlCompleter,
}

const SQL_KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "LIKE", "AND", "OR", "NOT", "IN", "ON",
    "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "CROSS", "GROUP", "BY",
    "ORDER", "ASC", "DESC", "LIMIT", "OFFSET", "INSERT", "UPDATE", "DELETE",
    "CREATE", "ALTER", "DROP", "TABLE", "INDEX", "VALUES", "SET",
    "INTO", "AS", "DISTINCT", "COUNT", "SUM", "AVG", "MIN", "MAX",
    "BETWEEN", "EXISTS", "IS", "NULL", "HAVING", "UNION", "ALL",
    "USE", "DATABASE", "SHOW", "DESCRIBE", "EXPLAIN", "CASE", "WHEN",
    "THEN", "ELSE", "END", "IF", "TRUE", "FALSE",
    "FETCH", "NEXT", "ROWS", "ONLY", "FOR",
    "SHARE", "LOCK", "NOWAIT", "SKIP", "LOCKED",
    "NATURAL", "USING", "DUPLICATE", "KEY", "REPLACE",
    "TRUNCATE", "CASCADE", "RESTRICT", "CONSTRAINT", "PRIMARY",
    "FOREIGN", "REFERENCES", "UNIQUE", "CHECK", "DEFAULT",
    "AUTO_INCREMENT", "GENERATED", "ALWAYS", "STORED", "VIRTUAL",
    "COMMENT", "COLUMN", "ADD", "MODIFY", "RENAME", "TO",
    "WITH", "RECURSIVE", "LATERAL",
];

const BUILDIN_COMMAND: &[&str] = &[
    "help", "quit", "exit", "clear", "status", "use",
    "source", "connect", "pager", "nopager", "ego", "color", "newline",
];

/// 历史记录配置
struct HistoryConfig {
    /// 最大保存条数
    max_entries: usize,
    /// 搜索时显示的最大条数
    search_display_limit: usize,
    /// 自动清理阈值（超过时触发清理）
    cleanup_threshold: usize,
    /// 每N条命令保存一次
    save_interval: usize,
    /// 保存时间间隔（秒）
    save_interval_secs: u64,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            max_entries: 1000,
            search_display_limit: 100,
            cleanup_threshold: 1200,
            save_interval: 10,
            save_interval_secs: 300,  // 5分钟
        }
    }
}

/// 历史记录管理器
struct HistoryManager {
    /// 历史记录文件路径
    file_path: PathBuf,
    /// 锁文件路径
    lock_path: PathBuf,
    /// 配置
    config: HistoryConfig,
    /// 内存中的历史记录
    entries: Vec<String>,
    /// 命令计数器（用于定期保存）
    command_count: usize,
    /// 上次保存时间戳
    last_save_time: u64,
}

impl HistoryManager {
    /// 创建新的历史记录管理器
    fn new(file_path: PathBuf, config: HistoryConfig) -> Self {
        let lock_path = file_path.with_extension("history.lock");
        
        Self {
            file_path,
            lock_path,
            config,
            entries: Vec::new(),
            command_count: 0,
            last_save_time: 0,
        }
    }
    
    /// 初始化：加载历史记录
    fn init(&mut self) -> Result<(), Box<dyn Error>> {
        if self.file_path.exists() {
            let file = OpenOptions::new()
                .read(true)
                .open(&self.file_path)?;
            
            let reader = BufReader::new(file);
            self.entries = reader
                .lines()
                .filter_map(|l| l.ok())
                .filter(|l| !l.trim().is_empty())
                .collect();
            
            // 限制加载条数
            if self.entries.len() > self.config.max_entries {
                let split_at = self.entries.len() - self.config.max_entries;
                self.entries = self.entries.split_off(split_at);
            }
        }
        
        // 初始化上次保存时间
        self.last_save_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        Ok(())
    }
    
    /// 添加命令到历史记录
    fn add_command(&mut self, command: &str) {
        // 过滤空命令
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return;
        }
        
        // 避免连续重复相同的命令
        if self.entries.last().map_or(true, |last| last != trimmed) {
            self.entries.push(trimmed.to_string());
        }
        
        self.command_count += 1;
        
        // 检查是否需要保存
        if self.should_save() {
            self.save().ok();  // 保存失败不中断程序
        }
    }
    
    /// 判断是否需要保存
    fn should_save(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        // 条件1：达到保存间隔
        if self.command_count >= self.config.save_interval {
            return true;
        }
        
        // 条件2：超过保存时间间隔
        if now.saturating_sub(self.last_save_time) >= self.config.save_interval_secs {
            return true;
        }
        
        // 条件3：超过清理阈值
        if self.entries.len() >= self.config.cleanup_threshold {
            return true;
        }
        
        false
    }
    
    /// 保存历史记录（使用非阻塞锁）
    fn save(&mut self) -> Result<(), Box<dyn Error>> {
        // 清理旧记录
        self.cleanup();
        
        // 尝试获取锁文件
        let lock_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.lock_path)?;
        
        // 使用非阻塞锁，获取不到就跳过本次保存
        let mut lock = RwLock::new(lock_file);
        let locked_file = match lock.try_write() {
            Ok(f) => f,
            Err(_) => return Ok(()),  // 其他进程正在写入，跳过
        };
        
        // 写入历史记录文件
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.file_path)?;
        
        for entry in &self.entries {
            writeln!(file, "{}", entry)?;
        }
        
        // 更新状态
        self.command_count = 0;
        self.last_save_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        // 释放锁（文件关闭时自动释放）
        drop(locked_file);
        
        Ok(())
    }
    
    /// 清理旧记录，只保留最后N条
    fn cleanup(&mut self) {
        if self.entries.len() > self.config.max_entries {
            let split_at = self.entries.len() - self.config.max_entries;
            self.entries = self.entries.split_off(split_at);
        }
    }
    
    /// 强制保存（退出时调用）
    fn force_save(&mut self) -> Result<(), Box<dyn Error>> {
        self.cleanup();
        
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.file_path)?;
        
        for entry in &self.entries {
            writeln!(file, "{}", entry)?;
        }
        
        // 删除锁文件
        remove_file(&self.lock_path).ok();
        
        Ok(())
    }
    
    /// 获取最后N条历史记录（用于加载到rustyline）
    fn get_recent(&self, limit: usize) -> Vec<String> {
        self.entries.iter()
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
    
}

impl MySQLClient {
    fn new(opts: &Opts) -> Result<Self, Box<dyn Error>> {
        let builder = OptsBuilder::new()
            .user(opts.user.as_deref())
            .pass(opts.password.as_deref())
            .ip_or_hostname(Some(opts.host.as_str()))
            .tcp_port(opts.port)
            .db_name(opts.database.as_deref());

        let mut conn = Conn::new(builder)?;
        let current_db = opts.database.clone();
        let use_colors = !opts.no_color_output;
        let host = opts.host.clone();
        let port = opts.port;

        let pattern = format!("(?i)\\b({})\\b", SQL_KEYWORDS.join("|"));
        let sql_keyword_re = Regex::new(&pattern).unwrap();

        let completer = SqlCompleter::new(&mut conn, &opts.database);

        Ok(MySQLClient {
            conn,
            current_db,
            use_colors,
            host,
            port,
            display_mode: if opts.no_vertical_output { DisplayMode::Mline(1) } else { DisplayMode::Vertical },
            color_output: !opts.no_color_output,
            highlight_color: "red".to_string(),
            oneline_mode: true,
            sql_keyword_re,
            pager_command: Some("less -R".to_string()),
            completer,
        })
    }

    fn highlight_sql_keywords(&self, query: &str) -> String {
        if !self.use_colors {
            return query.to_string();
        }
        let mut result = String::new();
        let mut last_end = 0;
        for cap in self.sql_keyword_re.captures_iter(query) {
            let m = cap.get(0).unwrap();
            result.push_str(&query[last_end..m.start()]);
            result.push_str(&m.as_str().bright_green().to_string());
            last_end = m.end();
        }
        result.push_str(&query[last_end..]);
        result
    }

    fn extract_where_keywords(query: &str) -> Vec<String> {
        let upper = query.to_uppercase();
        let where_pos = match upper.find(" WHERE ") {
            Some(p) => p + 7,
            None => return Vec::new(),
        };
        let where_clause = &query[where_pos..];

        let mut keywords = Vec::new();
        let bytes = where_clause.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\'' => {
                    i += 1;
                    let start = i;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    let val = &where_clause[start..i];
                    let cleaned = val.trim_matches('%');
                    if !cleaned.is_empty() {
                        keywords.push(cleaned.to_string());
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                }
                b'"' => {
                    i += 1;
                    let start = i;
                    while i < bytes.len() && bytes[i] != b'"' {
                        if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    let val = &where_clause[start..i];
                    let cleaned = val.trim_matches('%');
                    if !cleaned.is_empty() {
                        keywords.push(cleaned.to_string());
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                }
                _ => {
                    i += 1;
                }
            }
        }
        keywords
    }

    fn highlight_keywords(&self, value: &str, keywords: &[String]) -> String {
        if keywords.is_empty() {
            return value.to_string();
        }

        let mut result = value.to_string();
        for kw in keywords {
            if !kw.is_empty() && result.contains(kw.as_str()) {
                let highlighted = match self.highlight_color.as_str() {
                    "green" => kw.green().to_string(),
                    "red" => kw.red().to_string(),
                    _ => kw.green().to_string(),
                };
                result = result.replace(kw.as_str(), &highlighted);
            }
        }
        result
    }

    fn format_cell(&self, value: String, is_null: bool, keywords: &[String], apply_color: bool) -> String {
        if is_null {
            if self.use_colors {
                "NULL".bright_red().to_string()
            } else {
                "NULL".to_string()
            }
        } else if apply_color && self.use_colors {
            self.highlight_keywords(&value, keywords)
        } else {
            value
        }
    }

    fn execute_query(&mut self, query: &str) -> Result<Option<QueryResult>, Box<dyn Error>> {
        let trimmed = query.trim();
        let stripped = trimmed.trim_end_matches(';').trim();
        let lower = stripped.to_lowercase();

        match lower.as_str() {
            "quit" | "exit" => {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "__EXIT__",
                )));
            }
            "status" | "\\s" => return self.show_status(),
            "clear" | "\\c" => {
                print!("\x1B[2J\x1B[1;1H");
                return Ok(None);
            }
            "help" | "\\h" | "\\?" | "?" => {
                self.show_help();
                return Ok(None);
            }
            "source" => {
                println!("{}", if self.use_colors {
                    "Usage: \\. <filename>".bright_yellow().to_string()
                } else {
                    "Usage: \\. <filename>".to_string()
                });
                return Ok(None);
            }
            "connect" | "\\r" => {
                println!("{}", if self.use_colors {
                    "Reconnecting...".bright_yellow().to_string()
                } else {
                    "Reconnecting...".to_string()
                });
                return Ok(None);
            }
            "tee" | "\\T" => {
                println!("{}", if self.use_colors {
                    "Note: TEE logging not implemented yet.".bright_yellow().to_string()
                } else {
                    "Note: TEE logging not implemented yet.".to_string()
                });
                return Ok(None);
            }
            "notee" | "\\t" => {
                return Ok(None);
            }
            "warnings" | "\\w" => {
                println!("{}", if self.use_colors {
                    "Show warnings enabled.".bright_yellow().to_string()
                } else {
                    "Show warnings enabled.".to_string()
                });
                return Ok(None);
            }
            "nowarning" | "\\W" => {
                println!("{}", if self.use_colors {
                    "Show warnings disabled.".bright_yellow().to_string()
                } else {
                    "Show warnings disabled.".to_string()
                });
                return Ok(None);
            }
            "charset" | "\\C" => {
                println!("{}", if self.use_colors {
                    "Usage: charset charset_name".bright_yellow().to_string()
                } else {
                    "Usage: charset charset_name".to_string()
                });
                return Ok(None);
            }
            "pager" | "\\P" => {
                let current = self.pager_command.as_deref().unwrap_or("none");
                let msg = format!("Current pager: {}\nAvailable pagers: none, less", current);
                println!("{}", if self.use_colors { msg.green().to_string() } else { msg });
                return Ok(None);
            }
            _ if lower.starts_with("pager ") => {
                let arg = stripped[6..].trim();
                if arg.is_empty() || arg == "none" {
                    self.pager_command = None;
                } else {
                    let pager_cmd = match arg {
                        "more" | "less" => "less -R",
                        other => other,
                    };
                    self.pager_command = Some(pager_cmd.to_string());
                }
                let current = self.pager_command.as_deref().unwrap_or("none");
                let msg = format!("Pager set to '{}'.", current);
                println!("{}", if self.use_colors { msg.green().to_string() } else { msg });
                return Ok(None);
            }
            "nopager" | "\\n" => {
                self.pager_command = None;
                println!("{}", if self.use_colors {
                    "Pager disabled.".green().to_string()
                } else {
                    "Pager disabled.".to_string()
                });
                return Ok(None);
            }
            _ if lower.starts_with("color ") => {
                let color = stripped[6..].trim().to_lowercase();
                match color.as_str() {
                    "green" | "red" => {
                        self.highlight_color = color.clone();
                        let msg = format!("Highlight color set to '{}'.", color);
                        println!("{}", if self.use_colors { msg.green().to_string() } else { msg });
                    }
                    _ => {
                        let msg = format!("Current highlight color: {}\nAvailable colors: green, red", self.highlight_color);
                        println!("{}", if self.use_colors { msg.green().to_string() } else { msg });
                    }
                }
                return Ok(None);
            }
            "color" => {
                let msg = format!("Current highlight color: {}\nAvailable colors: green, red\nUsage: color [green|red]", self.highlight_color);
                println!("{}", if self.use_colors { msg.green().to_string() } else { msg });
                return Ok(None);
            }
            _ if lower.starts_with("newline ") => {
                let arg = stripped[8..].trim().to_lowercase();
                match arg.as_str() {
                    "oneline" => {
                        self.oneline_mode = true;
                        println!("{}", if self.use_colors {
                            "Input mode: oneline (auto-exec on Enter)".green().to_string()
                        } else {
                            "Input mode: oneline (auto-exec on Enter)".to_string()
                        });
                    }
                    "multiple" => {
                        self.oneline_mode = false;
                        println!("{}", if self.use_colors {
                            "Input mode: multiple (execute on ;)".green().to_string()
                        } else {
                            "Input mode: multiple (execute on ;)".to_string()
                        });
                    }
                    _ => {
                        let mode = if self.oneline_mode { "oneline" } else { "multiple" };
                        let msg = format!("Current mode: {}\nAvailable modes: oneline, multiple\nUsage: newline [oneline|multiple]", mode);
                        println!("{}", if self.use_colors { msg.green().to_string() } else { msg });
                    }
                }
                return Ok(None);
            }
            "newline" => {
                let mode = if self.oneline_mode { "oneline" } else { "multiple" };
                let msg = format!("Current mode: {}\nAvailable modes: oneline, multiple\nUsage: newline [oneline|multiple]", mode);
                println!("{}", if self.use_colors { msg.green().to_string() } else { msg });
                return Ok(None);
            }
            _ if lower.starts_with("ego ") || lower == "ego" || lower == "\\g" => {
                let current_mode_str = match self.display_mode {
                    DisplayMode::Vertical => "vertical".to_string(),
                    DisplayMode::Mline(n) => format!("line({})", n),
                    DisplayMode::Raw => "raw".to_string(),
                };
                let arg = stripped.split_whitespace().nth(1);
                match arg {
                    Some("v") | Some("vertical") => {
                        self.display_mode = DisplayMode::Vertical;
                        let msg = format!("Display mode: vertical (was: {}, available: vertical, line [N], raw)", current_mode_str);
                        println!("{}", if self.use_colors { msg.green().to_string() } else { msg });
                    }
                    Some("l") | Some("line") | Some("m") | Some("mline") => {
                        let n = stripped.split_whitespace().nth(2)
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(1)
                            .min(10).max(1);
                        self.display_mode = DisplayMode::Mline(n);
                        let msg = format!("Display mode: line({}) (was: {}, available: vertical, line [N], raw)", n, current_mode_str);
                        println!("{}", if self.use_colors { msg.green().to_string() } else { msg });
                    }
                    Some("r") | Some("raw") => {
                        self.display_mode = DisplayMode::Raw;
                        let msg = format!("Display mode: raw (was: {}, available: vertical, line [N], raw)", current_mode_str);
                        println!("{}", if self.use_colors { msg.green().to_string() } else { msg });
                    }
                    None => {
                        self.display_mode = match self.display_mode {
                            DisplayMode::Vertical => DisplayMode::Mline(1),
                            DisplayMode::Mline(_) => DisplayMode::Raw,
                            DisplayMode::Raw => DisplayMode::Vertical,
                        };
                        let new_mode_str = match self.display_mode {
                            DisplayMode::Vertical => "vertical".to_string(),
                            DisplayMode::Mline(n) => format!("line({})", n),
                            DisplayMode::Raw => "raw".to_string(),
                        };
                        let msg = format!("Display mode: {} (was: {}, available: vertical, line [N], raw)", new_mode_str, current_mode_str);
                        println!("{}", if self.use_colors { msg.green().to_string() } else { msg });
                    }
                    _ => {
                        println!("{}", if self.use_colors {
                            "Usage: ego [vertical|line [N]|raw]".bright_yellow().to_string()
                        } else {
                            "Usage: ego [vertical|line [N]|raw]".to_string()
                        });
                    }
                }
                return Ok(None);
            }
            _ => {}
        }

        let start_time = std::time::Instant::now();
        let use_colors = self.use_colors;
        let color_output = self.color_output;

        if lower.starts_with("use ") {
            let db = stripped[4..].trim().trim_matches(';').trim();
            self.conn.select_db(db)?;
            self.current_db = Some(db.to_string());
            self.completer.refresh(&mut self.conn, &self.current_db);

            let msg = format!("Database changed to '{}'", db);
            println!("{}", if use_colors { msg.green().to_string() } else { msg });

            return Ok(None);
        }

        let is_select = lower.starts_with("select");
        let keywords = if color_output && is_select {
            Self::extract_where_keywords(stripped)
        } else {
            Vec::new()
        };

        let rows: Vec<mysql::Row> = self.conn.query(query)?;

        let column_info = rows.first()
            .map(|r| r.columns_ref().to_vec())
            .unwrap_or_default();

        if column_info.is_empty() {
            let elapsed = start_time.elapsed();
            if is_select {
                let msg = format!("Empty set ({:.2} sec)", elapsed.as_secs_f64());
                println!("{}", if use_colors { msg.green().to_string() } else { msg });
            } else {
                let affected_rows = self.conn.affected_rows();
                if affected_rows > 0 {
                    let msg = format!(
                        "Query OK, {} {} affected ({:.2} sec)",
                        affected_rows,
                        if affected_rows == 1 { "row" } else { "rows" },
                        elapsed.as_secs_f64()
                    );
                    println!("{}", if use_colors { msg.green().to_string() } else { msg });
                }
            }
            return Ok(None);
        }

        let num_cols = column_info.len();
        let term_width = term_size::dimensions()
            .map(|(w, _)| w)
            .unwrap_or(120);

        let mut col_widths: Vec<usize> = column_info.iter()
            .map(|c| display_width(&c.name_str()))
            .collect();

        for row in &rows {
            for i in 0..num_cols {
                if i < col_widths.len() {
                    let (value, _) = Self::get_cell_value(row, i);
                    col_widths[i] = col_widths[i].max(display_width(&value));
                }
            }
        }

        let total_separators = num_cols * 3 + 1;
        let header_widths: Vec<usize> = column_info.iter()
            .map(|c| display_width(&c.name_str()))
            .collect();
        let header_total: usize = header_widths.iter().sum();
        let available = term_width.saturating_sub(total_separators);

        if header_total >= available {
            let ratio = available as f64 / header_total as f64;
            for (w, &hw) in col_widths.iter_mut().zip(header_widths.iter()) {
                *w = (hw as f64 * ratio).max(2.0) as usize;
            }
        } else {
            for (w, &hw) in col_widths.iter_mut().zip(header_widths.iter()) {
                *w = hw;
            }
            let remaining = available.saturating_sub(header_total);
            let content_extra: usize = col_widths.iter().zip(header_widths.iter())
                .map(|(c, h)| c.saturating_sub(*h))
                .collect::<Vec<_>>().iter().sum();
            if content_extra > 0 {
                for (w, &hw) in col_widths.iter_mut().zip(header_widths.iter()) {
                    let extra_for_this = w.saturating_sub(hw);
                    let share = extra_for_this as f64 / content_extra as f64;
                    *w += (remaining as f64 * share) as usize;
                }
            } else {
                let per = remaining / num_cols;
                for w in col_widths.iter_mut() {
                    *w += per;
                }
            }
            let used: usize = col_widths.iter().sum();
            let mut leftover = available.saturating_sub(used);
            for w in col_widths.iter_mut() {
                if leftover > 0 {
                    *w += 1;
                    leftover -= 1;
                }
            }
        }

        match self.display_mode {
            DisplayMode::Vertical => {
                let mut lines: Vec<String> = Vec::new();
                let row_count = rows.len();

                for (idx, row) in rows.iter().enumerate() {
                    let divider = format!("***************************[ {}. row ]***************************", idx + 1);
                    if use_colors {
                        lines.push(divider.bright_cyan().to_string());
                    } else {
                        lines.push(divider);
                    }

                    for (i, col) in column_info.iter().enumerate() {
                        let field_name = col.name_str().to_string();
                        let (value, is_null) = Self::get_cell_value(row, i);

                        let formatted = if use_colors {
                            let field_display = format!("{:<width$}", field_name, width = 10);
                            let field_colored = field_display.bright_cyan().to_string();
                            let value_display = self.format_cell(value, is_null, &keywords, color_output);
                            format!("{} | {}", field_colored, value_display)
                        } else {
                            format!("{:<width$} | {}", field_name, value, width = 10)
                        };
                        lines.push(formatted);
                    }
                    lines.push(String::new());
                }

                let elapsed = start_time.elapsed();
                let summary = format!(
                    "{} {} in set ({:.2} sec)",
                    row_count,
                    if row_count == 1 { "row" } else { "rows" },
                    elapsed.as_secs_f64()
                );

                return Ok(Some(QueryResult {
                    table: None,
                    lines,
                    summary,
                    is_vertical: true,
                }));
            }
            DisplayMode::Mline(max_lines) => {
                let row_count = rows.len();

                let header_truncated: Vec<String> = column_info.iter().enumerate()
                    .map(|(i, c)| truncate_str(&c.name_str(), col_widths[i]))
                    .collect();

                let data_wraps: Vec<Vec<Vec<String>>> = rows.iter().map(|row| {
                    (0..num_cols).map(|i| {
                        let (value, _) = Self::get_cell_value(row, i);
                        let all_lines = wrap_text(&value, col_widths[i]);
                        let truncated = all_lines.len() > max_lines;
                        let mut lines: Vec<String> = all_lines.into_iter().take(max_lines).collect();
                        if truncated {
                            if let Some(last) = lines.last_mut() {
                                let w = display_width(last);
                                if w >= col_widths[i] && col_widths[i] >= 2 {
                                    let chars: Vec<char> = last.chars().collect();
                                    let mut width = 0;
                                    let mut end = 0;
                                    for (idx, ch) in chars.iter().enumerate() {
                                        let cw = unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0);
                                        if width + cw > col_widths[i] - 1 {
                                            break;
                                        }
                                        width += cw;
                                        end = idx + 1;
                                    }
                                    *last = format!("{}…", chars[..end].iter().collect::<String>());
                                } else if !last.is_empty() {
                                    last.push('…');
                                }
                            }
                        }
                        lines
                    }).collect()
                }).collect();

                let mut out = String::new();

                out.push_str(&format!("┌{}┐\n",
                    col_widths.iter().enumerate().map(|(i, w)| {
                        format!("{}{}", "─".repeat(*w + 2), if i < num_cols - 1 { "┬" } else { "" })
                    }).collect::<Vec<_>>().join("")
                ));

                out.push('│');
                for (i, name) in header_truncated.iter().enumerate() {
                    let pad = " ".repeat(col_widths[i].saturating_sub(display_width(name)));
                    if use_colors {
                        out.push_str(&format!(" {}{} │", name.bright_cyan(), pad));
                    } else {
                        out.push_str(&format!(" {}{} │", name, pad));
                    }
                }
                out.push('\n');

                for (_ri, row_wraps) in data_wraps.iter().enumerate() {
                    out.push_str(&format!("├{}┤\n",
                        col_widths.iter().enumerate().map(|(i, w)| {
                            format!("{}{}", "─".repeat(*w + 2), if i < num_cols - 1 { "┼" } else { "" })
                        }).collect::<Vec<_>>().join("")
                    ));

                    let row_max = row_wraps.iter().map(|l| l.len()).max().unwrap_or(1);
                    for li in 0..row_max {
                        out.push('│');
                        for (i, lines) in row_wraps.iter().enumerate() {
                            let content = if li < lines.len() { &lines[li] } else { "" };
                            let display = truncate_str(content, col_widths[i]);
                            let value = display.clone();
                            let is_null = false;
                            let formatted = self.format_cell(value, is_null, &keywords, color_output);
                            let raw_len = display_width(&display);
                            let pad = " ".repeat(col_widths[i].saturating_sub(raw_len));
                            out.push_str(&format!(" {}{} │", formatted, pad));
                        }
                        out.push('\n');
                    }
                }

                out.push_str(&format!("└{}┘\n",
                    col_widths.iter().enumerate().map(|(i, w)| {
                        format!("{}{}", "─".repeat(*w + 2), if i < num_cols - 1 { "┴" } else { "" })
                    }).collect::<Vec<_>>().join("")
                ));

                let elapsed = start_time.elapsed();
                let summary = format!(
                    "{} {} in set ({:.2} sec)",
                    row_count,
                    if row_count == 1 { "row" } else { "rows" },
                    elapsed.as_secs_f64()
                );

                if use_colors {
                    out.push_str(&format!("\n{}", summary.green()));
                } else {
                    out.push_str(&format!("\n{}", summary));
                }

                return Ok(Some(QueryResult {
                    table: None,
                    lines: vec![out],
                    summary: String::new(),
                    is_vertical: true,
                }));
            }
            DisplayMode::Raw => {
                let row_count = rows.len();
                let mut out = String::new();
                
                // 输出表头（字段名用|分隔）
                let header: Vec<String> = column_info.iter()
                    .map(|c| c.name_str().to_string())
                    .collect();
                if use_colors {
                    out.push_str(&header.join("|").bright_cyan().to_string());
                } else {
                    out.push_str(&header.join("|"));
                }
                out.push('\n');
                
                // 输出每行数据（字段值用|分隔）
                for row in &rows {
                    let values: Vec<String> = (0..num_cols)
                        .map(|i| {
                            let (value, is_null) = Self::get_cell_value(row, i);
                            if is_null {
                                "NULL".to_string()
                            } else {
                                value
                            }
                        })
                        .collect();
                    out.push_str(&values.join("|"));
                    out.push('\n');
                }
                
                let elapsed = start_time.elapsed();
                let summary = format!(
                    "{} {} in set ({:.2} sec)",
                    row_count,
                    if row_count == 1 { "row" } else { "rows" },
                    elapsed.as_secs_f64()
                );
                
                if use_colors {
                    out.push_str(&format!("\n{}", summary.green()));
                } else {
                    out.push_str(&format!("\n{}", summary));
                }
                
                return Ok(Some(QueryResult {
                    table: None,
                    lines: vec![out],
                    summary: String::new(),
                    is_vertical: true,
                }));
            }
        }
    }

    fn get_cell_value(row: &mysql::Row, idx: usize) -> (String, bool) {
        match row.get_opt(idx) {
            Some(Ok(val)) => match val {
                Value::NULL => ("NULL".to_string(), true),
                Value::Bytes(bytes) => (String::from_utf8_lossy(&bytes).into_owned(), false),
                Value::Int(n) => (n.to_string(), false),
                Value::UInt(n) => (n.to_string(), false),
                Value::Float(f) => (f.to_string(), false),
                Value::Double(d) => (d.to_string(), false),
                Value::Date(y, m, d, h, i, s, _) =>
                    (format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, h, i, s), false),
                Value::Time(neg, d, h, i, s, _) => {
                    let sign = if neg { "-" } else { "" };
                    (format!("{}{}.{:02}:{:02}:{:02}", sign, d, h, i, s), false)
                }
            },
            _ => ("NULL".to_string(), true),
        }
    }

    fn show_help(&self) {
        let use_colors = self.use_colors;
        let help_text = vec![
            ("help (\\h, \\?, ?)", "Display this help"),
            ("quit (exit)", "Exit the client"),
            ("clear (\\c)", "Clear the current input statement"),
            ("status (\\s)", "Display current connection status"),
            ("use <db>", "Change the current database"),
            ("source (\\.)", "Execute script file. Usage: \\. <filename>"),
            ("connect (\\r)", "Reconnect to the server"),
            ("pager (\\P)", "Set pager (none/less). Usage: pager less"),
            ("nopager (\\n)", "Disable pager"),
            ("ego (\\G)", "Switch display mode: vertical|line [N]|raw (e.g. ego raw)"),
            ("color", "Set highlight color: green|red (default: red)"),
            ("newline", "Set input mode: oneline|multiple (default: oneline)"),
        ];

        let extra_text = vec![
            ("SQL || /path/to/file", "Execute SQL and output to file (append)"),
        ];

        println!();
        if use_colors {
            println!("{}", "List of all MySQL client commands:".bright_green());
            println!();
        } else {
            println!("List of all MySQL client commands:");
            println!();
        }

        let max_len = help_text.iter().map(|(cmd, _)| cmd.len()).max().unwrap_or(0);
        for (cmd, desc) in &help_text {
            if use_colors {
                println!("  {:<width$}  {}", cmd.bright_cyan(), desc, width = max_len);
            } else {
                println!("  {:<width$}  {}", cmd, desc, width = max_len);
            }
        }
        
        println!();
        if use_colors {
            println!("{}", "SQL output redirection:".bright_green());
            println!();
        } else {
            println!("SQL output redirection:");
            println!();
        }
        
        let max_len_extra = extra_text.iter().map(|(cmd, _)| cmd.len()).max().unwrap_or(0);
        for (cmd, desc) in &extra_text {
            if use_colors {
                println!("  {:<width$}  {}", cmd.bright_cyan(), desc, width = max_len_extra);
            } else {
                println!("  {:<width$}  {}", cmd, desc, width = max_len_extra);
            }
        }
        println!();
    }

    fn show_status(&mut self) -> Result<Option<QueryResult>, Box<dyn Error>> {
        let mut table = Table::new();
        let fmt = format::FormatBuilder::new()
            .column_separator(' ')
            .borders(' ')
            .padding(1, 1)
            .build();
        table.set_format(fmt);

        let server_version: String = self.conn.query_first("SELECT VERSION()")?.unwrap_or_default();
        table.add_row(PrettyRow::new(vec![
            Cell::new("Server version:").style_spec("Fb"),
            Cell::new(&server_version),
        ]));

        table.add_row(PrettyRow::new(vec![
            Cell::new("Server:").style_spec("Fb"),
            Cell::new(&format!("{}:{}", self.host, self.port)),
        ]));

        table.add_row(PrettyRow::new(vec![
            Cell::new("Current database:").style_spec("Fb"),
            Cell::new(self.current_db.as_deref().unwrap_or("None")),
        ]));

        let charset: String = self.conn.query_first("SELECT @@character_set_client")?.unwrap_or_default();
        table.add_row(PrettyRow::new(vec![
            Cell::new("Character set:").style_spec("Fb"),
            Cell::new(&charset),
        ]));

        Ok(Some(QueryResult {
            table: Some(table),
            summary: String::new(),
            is_vertical: false,
            lines: Vec::new(),
        }))
    }

    fn print_output(&self, output: &str) {
        match &self.pager_command {
            Some(cmd) => {
                let term_height = term_size::dimensions()
                    .map(|(_, h)| h)
                    .unwrap_or(24);
                let line_count = output.lines().count();

                if line_count >= term_height {
                    use std::io::Write;
                    use std::process::{Command, Stdio};
                    if let Ok(mut child) = Command::new("sh")
                        .args(["-c", cmd])
                        .stdin(Stdio::piped())
                        .stdout(Stdio::inherit())
                        .spawn()
                    {
                        if let Some(ref mut stdin) = child.stdin {
                            let _ = stdin.write_all(output.as_bytes());
                        }
                        let _ = child.wait();
                    }
                }
                print!("{}", output);
            }
            None => {
                print!("{}", output);
            }
        }
    }

    fn print_result(&self, result: &QueryResult) {
        let output = self.result_to_string(result);
        self.print_output(&output);
    }

    fn result_to_string(&self, result: &QueryResult) -> String {
        let mut buf = String::new();
        if result.is_vertical {
            for line in &result.lines {
                buf.push_str(line);
                buf.push('\n');
            }
        } else if let Some(ref table) = result.table {
            use std::fmt::Write;
            let _ = write!(buf, "{}", table);
        }
        if !result.summary.is_empty() {
            buf.push('\n');
            if self.use_colors {
                use std::fmt::Write;
                let _ = write!(buf, "\n{}", result.summary.green());
            } else {
                buf.push_str(&result.summary);
            }
        }
        buf
    }
}

struct QueryResult {
    table: Option<Table>,
    summary: String,
    is_vertical: bool,
    lines: Vec<String>,
}

fn print_welcome_message(client: &mut MySQLClient) {
    if let Ok(Some(version)) = client.conn.query_first::<String, _>("SELECT VERSION()") {
        let banner = format!(r#"
Welcome to the MySQL monitor.  Commands end with ;

Server version: {}
Connection Id: {}

Copyright (c) 2000, 2024, Oracle and/or its affiliates.
Rust MySQL Monitor. A cross-platform MySQL client.

Type 'help;' or '\h' for help. Type '\c' to clear the current input statement.
"#, version, client.conn.connection_id());

        if client.use_colors {
            println!("{}", banner.bright_blue());
        } else {
            println!("{}", banner);
        }
    }
}

fn format_prompt(client: &MySQLClient, is_continuation: bool) -> String {
    if is_continuation {
        if client.use_colors {
            "    -> ".bright_green().to_string()
        } else {
            "    -> ".to_string()
        }
    } else {
        let db_str = client.current_db
            .as_ref()
            .map(|db| format!("({})", db))
            .unwrap_or_default();

        if client.use_colors {
            format!("mysql{} > ", db_str).bright_green().to_string()
        } else {
            format!("mysql{} > ", db_str)
        }
    }
}

struct SqlHelper {
    completer: SqlCompleter,
}

impl rustyline::Helper for SqlHelper {}

impl Completer for SqlHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> Result<(usize, Vec<Pair>), ReadlineError> {
        self.completer.complete(line, pos, ctx)
    }
}

impl rustyline::highlight::Highlighter for SqlHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        let pattern = format!("(?i)\\b({})\\b", self.completer.keywords.join("|"));
        if let Ok(re) = regex::Regex::new(&pattern) {
            let highlighted = re.replace_all(line, |caps: &regex::Captures| {
                caps[0].green().to_string()
            });
            if highlighted != line {
                Cow::Owned(highlighted.into_owned())
            } else {
                Cow::Borrowed(line)
            }
        } else {
            Cow::Borrowed(line)
        }
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(&'s self, prompt: &'p str, _default: bool) -> Cow<'b, str> {
        Cow::Borrowed(prompt)
    }
}

impl rustyline::hint::Hinter for SqlHelper {
    type Hint = String;

    fn hint(&self, _line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<String> {
        None
    }
}

impl rustyline::validate::Validator for SqlHelper {}

fn main() -> Result<(), Box<dyn Error>> {
    let opts = Opts::from_args();

    if opts.help {
        Opts::clap().print_help()?;
        println!();
        return Ok(());
    }

    let mut client = MySQLClient::new(&opts)?;

    if let Some(query) = opts.execute {
        println!("{}", client.highlight_sql_keywords(&query));
        if let Some(result) = client.execute_query(&query)? {
            client.print_result(&result);
        }
        return Ok(());
    }

    let history_file = home_dir()
        .map(|mut path| {
            path.push(".mysql_history");
            path
        })
        .unwrap_or_else(|| PathBuf::from(".mysql_history"));

    // 初始化历史记录管理器
    let mut history_manager = HistoryManager::new(
        history_file.clone(),
        HistoryConfig::default(),
    );
    if let Err(e) = history_manager.init() {
        eprintln!("Warning: Failed to load history: {}", e);
    }

    let helper = SqlHelper { completer: client.completer.clone() };
    let config = rustyline::config::Config::builder()
        .completion_type(rustyline::config::CompletionType::Circular)
        .build();
    let mut rl = Editor::with_config(config)?;
    rl.set_helper(Some(helper));
    
    // 设置rustyline历史记录最大条数
    rl.set_max_history_size(history_manager.config.max_entries)?;
    
    // 加载最近的历史记录到rustyline（用于上下键导航）
    for entry in history_manager.get_recent(history_manager.config.search_display_limit) {
        rl.add_history_entry(&entry)?;
    }

    print_welcome_message(&mut client);

    let mut query_buffer = String::new();
    let exit_result: Result<(), Box<dyn Error>> = loop {
        let prompt = format_prompt(&client, !query_buffer.is_empty());

        match rl.readline(&prompt) {
            Ok(line) => {
                rl.add_history_entry(line.as_str())?;
                history_manager.add_command(&line);

                if line.trim().is_empty() {
                    continue;
                }

                let trimmed = line.trim().trim_end_matches(';').trim().to_lowercase();
                let is_internal = matches!(trimmed.as_str(),
                    "quit" | "exit" | "help" | "clear" | "\\c" |
                    "status" | "\\s" | "ego" | "\\g" |
                    "nopager" | "\\n" | "color" |
                    "source" | "\\." | "connect" | "\\r" |
                    "newline"
                ) || trimmed.starts_with("ego ")
                   || trimmed.starts_with("pager ")
                   || trimmed.starts_with("use ")
                   || trimmed.starts_with("color ")
                   || trimmed.starts_with("newline ");

                let effective_line = if is_internal && !line.trim().ends_with(';') {
                    format!("{};", line.trim())
                } else if client.oneline_mode && !is_internal && !line.trim().ends_with(';') {
                    format!("{};", line.trim())
                } else {
                    line.clone()
                };

                query_buffer.push_str(&effective_line);
                query_buffer.push(' ');

                if effective_line.trim().ends_with(';') {
                    // 检测 || filename 语法
                    let (sql_query, output_file) = if let Some(pos) = query_buffer.find("||") {
                        let sql_part = query_buffer[..pos].trim();
                        let file_part = query_buffer[pos + 2..].trim();
                        // 去掉文件名末尾的分号
                        let file_part = file_part.trim_end_matches(';').trim();
                        if !file_part.is_empty() {
                            (sql_part.to_string(), Some(file_part.to_string()))
                        } else {
                            (query_buffer.clone(), None)
                        }
                    } else {
                        (query_buffer.clone(), None)
                    };
                    
                    println!("{}", client.highlight_sql_keywords(&sql_query));
                    match client.execute_query(&sql_query) {
                        Ok(Some(result)) => {
                            if let Some(ref filename) = output_file {
                                // 输出到文件
                                use std::fs::OpenOptions;
                                use std::io::Write;
                                
                                match OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(filename)
                                {
                                    Ok(mut file) => {
                                        let output = client.result_to_string(&result);
                                        if let Err(e) = file.write_all(output.as_bytes()) {
                                            eprintln!("{}", if client.use_colors {
                                                format!("Error writing to file: {}", e).bright_red().to_string()
                                            } else {
                                                format!("Error writing to file: {}", e)
                                            });
                                        } else {
                                            let msg = format!("Output appended to '{}'", filename);
                                            println!("{}", if client.use_colors { msg.green().to_string() } else { msg });
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("{}", if client.use_colors {
                                            format!("Error creating file '{}': {}", filename, e).bright_red().to_string()
                                        } else {
                                            format!("Error creating file '{}': {}", filename, e)
                                        });
                                    }
                                }
                            } else {
                                client.print_result(&result);
                            }
                        }
                        Ok(None) => {}
                        Err(ref e) if e.downcast_ref::<std::io::Error>()
                            .map_or(false, |io_err| io_err.to_string() == "__EXIT__") =>
                        {
                            println!("Bye");
                            break Ok(());
                        }
                        Err(e) => eprintln!("{}", if client.use_colors {
                            format!("Error: {}", e).bright_red().to_string()
                        } else {
                            format!("Error: {}", e)
                        }),
                    }
                    query_buffer.clear();
                }
            }
            Err(ReadlineError::Interrupted) => {
                println!("^C");
                query_buffer.clear();
            }
            Err(ReadlineError::Eof) => {
                println!("Bye");
                break Ok(());
            }
            Err(err) => {
                println!("Error: {:?}", err);
                break Ok(());
            }
        }
    };

    // 强制保存历史记录
    if let Err(e) = history_manager.force_save() {
        eprintln!("Warning: Failed to save history: {}", e);
    }
    exit_result
}
