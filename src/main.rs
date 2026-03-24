use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal;
use memchr::memmem;
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};
use rayon::prelude::*;
use serde::Deserialize;

#[derive(Debug, Clone)]
struct Session {
    session_id: String,
    custom_title: String,
    project_path: String,
}

#[derive(Deserialize)]
struct JsonlEntry {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

fn claude_projects_dir() -> PathBuf {
    dirs::home_dir()
        .expect("HOME が見つかりません")
        .join(".claude")
        .join("projects")
}

/// エンコードされたプロジェクトディレクトリ名からファイルシステムパスを復元します。
/// 左から貪欲にディレクトリ名をマッチさせます。
/// `-` は `/` または `.` にエンコードされるため、両方の候補を試します。
fn decode_project_path(encoded: &str) -> Option<String> {
    let stripped = &encoded[1..];
    let parts: Vec<&str> = stripped.split('-').collect();

    let mut current = PathBuf::from("/");
    let mut i = 0;

    while i < parts.len() {
        if parts[i].is_empty() {
            i += 1;
            continue;
        }

        let mut found = false;
        let end_max = parts.len();
        for end in (i + 1..=end_max).rev() {
            // `-` をそのまま維持した候補
            let candidate_hyphen: String = parts[i..end].join("-");
            let full = current.join(&candidate_hyphen);
            if full.exists() {
                current = full;
                i = end;
                found = true;
                break;
            }

            // `-` を `.` に置換した候補（ドットを含むディレクトリ名に対応）
            let candidate_dot: String = parts[i..end].join(".");
            let full_dot = current.join(&candidate_dot);
            if full_dot.exists() {
                current = full_dot;
                i = end;
                found = true;
                break;
            }
        }

        if !found {
            current = current.join(parts[i]);
            i += 1;
        }
    }

    if current.exists() {
        Some(current.to_string_lossy().to_string())
    } else {
        None
    }
}

/// JSONL ファイルから custom-title 行だけを高速抽出します。
/// ファイル全体を読み込み、"custom-title" を含む行だけをデシリアライズします。
fn extract_sessions_from_file(path: &PathBuf, project_path: &str) -> Vec<(String, Session)> {
    let mut results = Vec::new();

    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return results,
    };

    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return results;
    }

    let needle = b"\"custom-title\"";
    let finder = memmem::Finder::new(needle);

    // needle を含む行だけを抽出してパースします
    let mut start = 0;
    while start < buf.len() {
        let end = memchr::memchr(b'\n', &buf[start..])
            .map(|p| start + p)
            .unwrap_or(buf.len());

        let line = &buf[start..end];

        if finder.find(line).is_some() {
            if let Ok(entry) = serde_json::from_slice::<JsonlEntry>(line) {
                if entry.entry_type.as_deref() == Some("custom-title") {
                    if let (Some(title), Some(sid)) = (entry.custom_title, entry.session_id) {
                        results.push((
                            sid.clone(),
                            Session {
                                session_id: sid,
                                custom_title: title,
                                project_path: project_path.to_string(),
                            },
                        ));
                    }
                }
            }
        }

        start = end + 1;
    }

    results
}

/// ~/.claude/projects/ を走査して named session の一覧を返します。
fn scan_sessions() -> Vec<Session> {
    let projects_dir = claude_projects_dir();

    let entries: Vec<_> = match fs::read_dir(&projects_dir) {
        Ok(e) => e.flatten().collect(),
        Err(_) => return Vec::new(),
    };

    // プロジェクトディレクトリごとに (dir_name, project_path, jsonl_paths) を収集します
    let work_items: Vec<(String, Vec<PathBuf>)> = entries
        .iter()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let dir_name = e.file_name().to_string_lossy().to_string();
            let project_dir = e.path();
            let jsonl_files: Vec<PathBuf> = fs::read_dir(&project_dir)
                .ok()?
                .flatten()
                .filter(|jf| {
                    jf.path()
                        .extension()
                        .and_then(|ext| ext.to_str())
                        == Some("jsonl")
                })
                .map(|jf| jf.path())
                .collect();
            if jsonl_files.is_empty() {
                None
            } else {
                Some((dir_name, jsonl_files))
            }
        })
        .collect();

    // rayon で並列にスキャンします
    let all_results: Vec<Vec<(String, Session)>> = work_items
        .par_iter()
        .filter_map(|(dir_name, jsonl_files)| {
            let project_path = decode_project_path(dir_name)?;
            let mut results = Vec::new();
            for path in jsonl_files {
                results.extend(extract_sessions_from_file(path, &project_path));
            }
            Some(results)
        })
        .collect();

    // 重複排除（同一 session_id は後勝ち）
    let mut sessions: HashMap<String, Session> = HashMap::new();
    for batch in all_results {
        for (sid, session) in batch {
            sessions.insert(sid, session);
        }
    }

    let mut result: Vec<Session> = sessions.into_values().collect();
    result.sort_by(|a, b| a.custom_title.cmp(&b.custom_title));
    result
}

/// fuzzy filter を適用して (元のインデックス, スコア) のペアを返します。
fn fuzzy_filter(sessions: &[Session], query: &str) -> Vec<(usize, u32)> {
    if query.is_empty() {
        return (0..sessions.len()).map(|i| (i, 0)).collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let pattern = Pattern::new(query, CaseMatching::Ignore, Normalization::Smart, AtomKind::Substring);

    let mut scored: Vec<(usize, u32)> = sessions
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let haystack = format!("{} {}", s.custom_title, s.project_path);
            let mut buf = Vec::new();
            pattern
                .score(nucleo_matcher::Utf32Str::new(&haystack, &mut buf), &mut matcher)
                .map(|score| (i, score))
        })
        .collect();

    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored
}

fn run_interactive(sessions: &[Session]) -> Option<&Session> {
    if sessions.is_empty() {
        eprintln!("named session が見つかりません。");
        return None;
    }

    let mut query = String::new();
    let mut selected: usize = 0;
    let mut filtered = fuzzy_filter(sessions, &query);

    terminal::enable_raw_mode().expect("raw mode の有効化に失敗しました");
    let mut stdout = io::stdout();

    let max_visible = 15usize;

    loop {
        let visible_count = filtered.len().min(max_visible);
        write!(stdout, "\r\x1b[J").ok();
        write!(stdout, "\x1b[36m> \x1b[0m{}\r\n", query).ok();
        write!(
            stdout,
            "\x1b[90m  {}/{} sessions\x1b[0m\r\n",
            filtered.len(),
            sessions.len()
        )
        .ok();

        for (vi, &(idx, _score)) in filtered.iter().take(visible_count).enumerate() {
            let s = &sessions[idx];
            if vi == selected {
                write!(
                    stdout,
                    "\x1b[7m  {} \x1b[90m({})\x1b[0m\x1b[7m\x1b[0m\r\n",
                    s.custom_title, s.project_path
                )
                .ok();
            } else {
                write!(
                    stdout,
                    "  {} \x1b[90m({})\x1b[0m\r\n",
                    s.custom_title, s.project_path
                )
                .ok();
            }
        }

        stdout.flush().ok();

        let lines_drawn = 2 + visible_count;
        write!(stdout, "\x1b[{}A", lines_drawn).ok();
        stdout.flush().ok();

        if let Ok(Event::Key(key)) = event::read() {
            match (key.code, key.modifiers) {
                (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    write!(stdout, "\r\x1b[J").ok();
                    stdout.flush().ok();
                    terminal::disable_raw_mode().ok();
                    return None;
                }
                (KeyCode::Enter, _) => {
                    if let Some(&(idx, _)) = filtered.get(selected) {
                        write!(stdout, "\r\x1b[J").ok();
                        stdout.flush().ok();
                        terminal::disable_raw_mode().ok();
                        return Some(&sessions[idx]);
                    }
                }
                (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                    if selected > 0 {
                        selected -= 1;
                    }
                }
                (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                    if selected + 1 < filtered.len().min(max_visible) {
                        selected += 1;
                    }
                }
                (KeyCode::Backspace, _) => {
                    query.pop();
                    filtered = fuzzy_filter(sessions, &query);
                    selected = 0;
                }
                (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                    query.push(c);
                    filtered = fuzzy_filter(sessions, &query);
                    selected = 0;
                }
                _ => {}
            }
        }
    }
}

fn exec_session(session: &Session) -> ! {
    env::set_current_dir(&session.project_path).unwrap_or_else(|e| {
        eprintln!("cd {} に失敗しました: {}", session.project_path, e);
        std::process::exit(1);
    });

    let err = Command::new("claude")
        .arg("--resume")
        .arg(&session.session_id)
        .exec();

    eprintln!("claude の起動に失敗しました: {}", err);
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let do_exec = args.iter().any(|a| a == "--exec" || a == "-e");

    if args.iter().any(|a| a == "--list") {
        let sessions = scan_sessions();
        for s in &sessions {
            println!("{}\t{}\t{}", s.session_id, s.custom_title, s.project_path);
        }
        return;
    }

    let sessions = scan_sessions();

    if let Some(session) = run_interactive(&sessions) {
        if do_exec {
            exec_session(session);
        } else {
            println!(
                "cd {} && claude --resume {}",
                shell_escape(&session.project_path),
                &session.session_id
            );
        }
    }
}

fn shell_escape(s: &str) -> String {
    if s.contains(' ') || s.contains('\'') || s.contains('"') || s.contains('\\') {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}
