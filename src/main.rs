use std::io::Write;
use std::process;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::collections::{HashSet, HashMap};
use std::sync::Mutex;
use rustyline::config::CompletionType;

#[derive(Debug, Clone)]
struct Job {
    job_number: u32,
    pid: u32,
    command: String,
    status: String,
}

lazy_static::lazy_static! {
    static ref COMPLETIONS: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());
    static ref JOBS: Mutex<Vec<Job>> = Mutex::new(Vec::new());
    static ref CHILD_PROCESSES: Mutex<HashMap<u32, Box<std::process::Child>>> = Mutex::new(HashMap::new());
    static ref HISTORY: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static ref LAST_APPENDED_INDEX: Mutex<usize> = Mutex::new(0);
}

/// Compute markers (+, -, or space) based on current job list
fn get_job_markers(jobs: &[Job]) -> HashMap<u32, &'static str> {
    let mut markers = HashMap::new();
    
    // Reverse order (most recent first)
    let active_or_reaped: Vec<u32> = jobs.iter().map(|j| j.pid).rev().collect();
    
    if let Some(&curr_pid) = active_or_reaped.get(0) {
        markers.insert(curr_pid, "+");
    }
    if let Some(&prev_pid) = active_or_reaped.get(1) {
        markers.insert(prev_pid, "-");
    }
    
    markers
}

/// Non-blockingly updates job statuses. Prints reaped jobs if `display_reaped` is true.
fn reap_jobs(display_reaped: bool) {
    let mut jobs = JOBS.lock().unwrap();
    let mut children = CHILD_PROCESSES.lock().unwrap();

    // 1. Update status for finished children
    for job in jobs.iter_mut() {
        if job.status == "Running" {
            if let Some(mut child) = children.remove(&job.pid) {
                match child.try_wait() {
                    Ok(Some(_status)) => {
                        job.status = "Done".to_string();
                    }
                    Ok(None) => {
                        children.insert(job.pid, child);
                    }
                    Err(_) => {
                        children.insert(job.pid, child);
                    }
                }
            }
        }
    }

    // 2. Determine job markers before printing/purging
    let markers = get_job_markers(&jobs);

    // 3. Display prompt-reaped jobs if requested
    if display_reaped {
        for job in jobs.iter() {
            if job.status == "Done" {
                let marker = markers.get(&job.pid).copied().unwrap_or(" ");
                println!(
                    "[{}]{}  {:<24}{}",
                    job.job_number, marker, job.status, job.command
                );
            }
        }
    }

    // 4. Remove 'Done' jobs if they were displayed
    if display_reaped {
        jobs.retain(|job| job.status != "Done");
    }
}

fn find_executables_in_path_matching(prefix: &str) -> Vec<String> {
    let mut executables = HashSet::new();
    
    if let Ok(path_var) = env::var("PATH") {
        let path_delimiter = if cfg!(windows) { ";" } else { ":" };
        
        for dir in path_var.split(path_delimiter) {
            if dir.is_empty() {
                continue;
            }
            
            let path = Path::new(dir);
            
            if !path.is_dir() {
                continue;
            }
            
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    if let Ok(file_name) = entry.file_name().into_string() {
                        if file_name.starts_with(prefix) {
                            if let Ok(metadata) = entry.metadata() {
                                if metadata.is_file() && is_executable(&entry.path()) {
                                    executables.insert(file_name);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    
    let mut result: Vec<String> = executables.into_iter().collect();
    result.sort();
    result
}

fn find_files_in_current_dir_matching(prefix: &str) -> Vec<(String, bool)> {
    let mut files = Vec::new();
    
    if let Ok(entries) = fs::read_dir(".") {
        for entry in entries.flatten() {
            if let Ok(file_name) = entry.file_name().into_string() {
                if file_name == "." || file_name == ".." {
                    continue;
                }
                if file_name.starts_with(prefix) {
                    let is_dir = entry.metadata().map(|m| m.is_dir()).unwrap_or(false);
                    files.push((file_name, is_dir));
                }
            }
        }
    }
    
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn find_files_in_path_matching(dir_path: &str, prefix: &str) -> Vec<(String, bool)> {
    let mut files = Vec::new();
    
    let path = Path::new(dir_path);
    if !path.is_dir() {
        return files;
    }
    
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(file_name) = entry.file_name().into_string() {
                if file_name == "." || file_name == ".." {
                    continue;
                }
                if file_name.starts_with(prefix) {
                    let is_dir = entry.metadata().map(|m| m.is_dir()).unwrap_or(false);
                    files.push((file_name, is_dir));
                }
            }
        }
    }
    
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn is_builtin(cmd: &str) -> bool {
    matches!(cmd, "echo" | "exit" | "type" | "pwd" | "cd" | "complete" | "jobs" | "history" | "declare")
}

fn find_executable_in_path(command: &str) -> Option<String> {
    if let Ok(path_var) = env::var("PATH") {
        let path_delimiter = if cfg!(windows) { ";" } else { ":" };
        for dir in path_var.split(path_delimiter) {
            let full_path = Path::new(dir).join(command);
            if full_path.exists() && is_executable(&full_path) {
                return Some(full_path.to_string_lossy().to_string());
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(path) {
            let mode = metadata.permissions().mode();
            (mode & 0o111) != 0
        } else {
            false
        }
    }
    #[cfg(windows)]
    {
        path.exists()
    }
}

fn invoke_completer(
    script_path: &str,
    cmd_parts: &[String],
    word_index: usize,
    comp_line: &str,
    comp_point: usize,
) -> Option<Vec<String>> {
    let mut command = process::Command::new(script_path);

    let command_name = &cmd_parts[0];
    let current_word = &cmd_parts[word_index];
    let previous_word = if word_index > 0 {
        &cmd_parts[word_index - 1]
    } else {
        ""
    };

    command
        .arg(command_name)
        .arg(current_word)
        .arg(previous_word);

    command.env("COMP_LINE", comp_line);
    command.env("COMP_POINT", comp_point.to_string());

    let output = command.output().ok()?;

    if !output.status.success() {
        eprintln!("Completer error: {}", String::from_utf8_lossy(&output.stderr));
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;

    let mut candidates: Vec<String> = stdout
        .lines()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect();
    
    candidates.sort();

    if candidates.is_empty() {
        None
    } else {
        Some(candidates)
    }
}

fn longest_common_prefix(strings: &[String]) -> String {
    if strings.is_empty() {
        return String::new();
    }
    
    if strings.len() == 1 {
        return strings[0].clone();
    }
    
    let mut lcp = String::new();
    let min_len = strings.iter().map(|s| s.len()).min().unwrap_or(0);
    
    for i in 0..min_len {
        let ch = strings[0].chars().nth(i).unwrap();
        if strings.iter().all(|s| s.chars().nth(i) == Some(ch)) {
            lcp.push(ch);
        } else {
            break;
        }
    }
    
    lcp
}

fn longest_common_prefix_files(files: &[(String, bool)]) -> String {
    if files.is_empty() {
        return String::new();
    }
    
    let names: Vec<String> = files.iter().map(|(name, _)| name.clone()).collect();
    longest_common_prefix(&names)
}

#[derive(Debug, Clone)]
struct Redirection {
    stdout_target: Option<(String, bool)>,
    stderr_target: Option<(String, bool)>,
}

fn parse_with_redirection(input: &str) -> (Vec<String>, Redirection) {
    let tokens = parse_command_with_quotes(input);
    let mut command_parts = Vec::new();
    let mut redirection = Redirection {
        stdout_target: None,
        stderr_target: None,
    };
    let mut i = 0;
    
    while i < tokens.len() {
        let token = &tokens[i];
        if token == ">>" || token == "1>>" {
            if i + 1 < tokens.len() {
                redirection.stdout_target = Some((tokens[i + 1].clone(), true));
                i += 2;
            } else {
                command_parts.push(token.clone());
                i += 1;
            }
        } else if token == ">" || token == "1>" {
            if i + 1 < tokens.len() {
                redirection.stdout_target = Some((tokens[i + 1].clone(), false));
                i += 2;
            } else {
                command_parts.push(token.clone());
                i += 1;
            }
        } else if token == "2>>" {
            if i + 1 < tokens.len() {
                redirection.stderr_target = Some((tokens[i + 1].clone(), true));
                i += 2;
            } else {
                command_parts.push(token.clone());
                i += 1;
            }
        } else if token == "2>" {
            if i + 1 < tokens.len() {
                redirection.stderr_target = Some((tokens[i + 1].clone(), false));
                i += 2;
            } else {
                command_parts.push(token.clone());
                i += 1;
            }
        } else {
            command_parts.push(token.clone());
            i += 1;
        }
    }
    (command_parts, redirection)
}

/// Split a command by pipes
fn split_by_pipes(parts: &[String]) -> Vec<Vec<String>> {
    let mut pipelines = Vec::new();
    let mut current_pipeline = Vec::new();
    
    for part in parts {
        if part == "|" {
            if !current_pipeline.is_empty() {
                pipelines.push(current_pipeline);
                current_pipeline = Vec::new();
            }
        } else {
            current_pipeline.push(part.clone());
        }
    }
    
    if !current_pipeline.is_empty() {
        pipelines.push(current_pipeline);
    }
    
    pipelines
}

fn execute_pipeline(pipelines: Vec<Vec<String>>, redirection: Redirection, run_background: bool, original_command: &str) -> Result<(), Box<dyn std::error::Error>> {
    if pipelines.is_empty() {
        return Ok(());
    }

    let mut children: Vec<process::Child> = Vec::new();
    let mut builtin_pipe_input: Option<Vec<u8>> = None;
    
    for (i, pipeline_parts) in pipelines.iter().enumerate() {
        if pipeline_parts.is_empty() {
            continue;
        }
        
        let cmd = &pipeline_parts[0];
        let args = &pipeline_parts[1..];
        
        // Check if this is a built-in command
        if is_builtin(cmd) {
            // Generate output from the built-in
            let output_text = match cmd.as_str() {
                "echo" => {
                    args.join(" ")
                }
                "type" => {
                    if args.is_empty() {
                        return Err("type: missing argument".into());
                    }
                    let target_cmd = &args[0];
                    if is_builtin(target_cmd) {
                        format!("{} is a shell builtin", target_cmd)
                    } else if let Some(full_path) = find_executable_in_path(target_cmd) {
                        format!("{} is {}", target_cmd, full_path)
                    } else {
                        format!("{}: not found", target_cmd)
                    }
                }
                "pwd" => {
                    match env::current_dir() {
                        Ok(path) => format!("{}", path.display()),
                        Err(e) => return Err(format!("pwd: {}", e).into()),
                    }
                }
                _ => {
                    return Err(format!("{}: not supported in pipeline", cmd).into());
                }
            };
            
            if i == pipelines.len() - 1 {
                // Last command in pipeline - output to redirection or stdout
                if let Some((filename, is_append)) = &redirection.stdout_target {
                    let mut file = fs::OpenOptions::new()
                        .create(true)
                        .append(*is_append)
                        .write(!is_append)
                        .truncate(!is_append)
                        .open(filename)?;
                    writeln!(file, "{}", output_text)?;
                } else {
                    println!("{}", output_text);
                }
            } else {
                // Not the last command - pass output to next command
                builtin_pipe_input = Some(format!("{}\n", output_text).into_bytes());
            }
        } else {
            // External command
            let mut command = process::Command::new(cmd);
            for arg in args {
                command.arg(arg);
            }
            
            // Set up stdin: if not the first command, pipe from previous
            if i > 0 {
                if let Some(_input_data) = builtin_pipe_input.as_ref() {
                    // Use piped stdin for builtin output
                    command.stdin(process::Stdio::piped());
                } else if let Some(prev_child) = children.pop() {
                    if let Some(stdout) = prev_child.stdout {
                        command.stdin(stdout);
                    }
                }
            }
            
            // Set up stdout: if not the last command, create a pipe
            if i < pipelines.len() - 1 {
                command.stdout(process::Stdio::piped());
            } else {
                // Last command: apply redirection if specified
                if let Some((filename, is_append)) = &redirection.stdout_target {
                    let file = fs::OpenOptions::new()
                        .create(true)
                        .append(*is_append)
                        .write(!is_append)
                        .truncate(!is_append)
                        .open(filename)?;
                    command.stdout(file);
                }
            }
            
            // Set up stderr redirection (only for last command)
            if i == pipelines.len() - 1 {
                if let Some((filename, is_append)) = &redirection.stderr_target {
                    let file = fs::OpenOptions::new()
                        .create(true)
                        .append(*is_append)
                        .write(!is_append)
                        .truncate(!is_append)
                        .open(filename)?;
                    command.stderr(file);
                }
            }
            
            let mut child = command.spawn()?;
            
            // Write builtin input if we have any
            if let Some(input_data) = builtin_pipe_input.take() {
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(&input_data)?;
                }
            }
            
            children.push(child);
        }
    }
    
    // Wait for all children if not running in background
    if run_background {
        if let Some(last_child) = children.pop() {
            let pid = last_child.id();
            
            let mut jobs = JOBS.lock().unwrap();
            
            let job_number = jobs
                .iter()
                .map(|j| j.job_number)
                .max()
                .map_or(1, |max_num| max_num + 1);
            
            let mut child_processes = CHILD_PROCESSES.lock().unwrap();
            child_processes.insert(pid, Box::new(last_child));
            
            let job_command = original_command.trim_end_matches('&').trim().to_string();
            let job = Job {
                job_number,
                pid,
                command: job_command,
                status: "Running".to_string(),
            };
            jobs.push(job);
            
            println!("[{}] {}", job_number, pid);
        }
    } else {
        for mut child in children {
            child.wait()?;
        }
    }
    
    Ok(())
}

fn execute_external_program(cmd: &str, args: &[String], redirection: Redirection, run_background: bool, original_command: &str) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(program_path) = find_executable_in_path(cmd) {
        let mut command = process::Command::new(&program_path);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.arg0(cmd);
        }
        for arg in args {
            command.arg(arg);
        }
        if let Some((filename, is_append)) = &redirection.stdout_target {
            let file = fs::OpenOptions::new().create(true).append(*is_append).write(!is_append).truncate(!is_append).open(filename)?;
            command.stdout(file);
        }
        if let Some((filename, is_append)) = &redirection.stderr_target {
            let file = fs::OpenOptions::new().create(true).append(*is_append).write(!is_append).truncate(!is_append).open(filename)?;
            command.stderr(file);
        }
        
        if run_background {
            let child = command.spawn()?;
            let pid = child.id();

            let mut jobs = JOBS.lock().unwrap();

            // Calculate recycled job number based on current jobs table
            let job_number = jobs
                .iter()
                .map(|j| j.job_number)
                .max()
                .map_or(1, |max_num| max_num + 1);

            let mut children = CHILD_PROCESSES.lock().unwrap();
            children.insert(pid, Box::new(child));
            
            let job_command = original_command.trim_end_matches('&').trim().to_string();
            let job = Job {
                job_number,
                pid,
                command: job_command,
                status: "Running".to_string(),
            };
            jobs.push(job);
            
            println!("[{}] {}", job_number, pid);
        } else {
            let mut child = command.spawn()?;
            child.wait()?;
        }
        Ok(())
    } else {
        Err(format!("{}: command not found", cmd).into())
    }
}

fn resolve_relative_path(target_dir: &str) -> PathBuf {
    let current_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut path = current_dir.clone();
    for component in target_dir.split('/') {
        match component {
            "" | "." => {}
            ".." => { path.pop(); }
            _ => { path.push(component); }
        }
    }
    path
}

fn expand_tilde(path: &str) -> String {
    if path.starts_with('~') {
        if let Ok(home) = env::var("HOME") {
            if path == "~" { home }
            else if path.starts_with("~/") { format!("{}{}", home, &path[1..]) }
            else { path.to_string() }
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    }
}

fn parse_command_with_quotes(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current_arg = String::new();
    let mut in_single_quotes = false;
    let mut in_double_quotes = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double_quotes => { in_single_quotes = !in_single_quotes; }
            '"' if !in_single_quotes => { in_double_quotes = !in_double_quotes; }
            '\\' if in_double_quotes => {
                if let Some(&next_ch) = chars.peek() {
                    match next_ch {
                        '"' | '\\' | '$' | '`' => {
                            chars.next();
                            current_arg.push(next_ch);
                        }
                        '\n' => { chars.next(); }
                        _ => { current_arg.push('\\'); }
                    }
                } else {
                    current_arg.push('\\');
                }
            }
            '\\' if !in_single_quotes && !in_double_quotes => {
                if let Some(next_ch) = chars.next() {
                    current_arg.push(next_ch);
                }
            }
            ' ' | '\t' => {
                if in_single_quotes || in_double_quotes {
                    current_arg.push(ch);
                } else if !current_arg.is_empty() {
                    args.push(current_arg.clone());
                    current_arg.clear();
                }
            }
            _ => { current_arg.push(ch); }
        }
    }
    if !current_arg.is_empty() {
        args.push(current_arg);
    }
    args
}

/// Load history from HISTFILE environment variable
fn load_history_from_file() {
    if let Ok(histfile) = env::var("HISTFILE") {
        match fs::read_to_string(&histfile) {
            Ok(content) => {
                let mut history = HISTORY.lock().unwrap();
                for line in content.lines() {
                    if !line.is_empty() {
                        history.push(line.to_string());
                    }
                }
                // Update last appended index to indicate all loaded entries are from file
                let mut last_appended = LAST_APPENDED_INDEX.lock().unwrap();
                *last_appended = history.len();
            }
            Err(_) => {
                // If file doesn't exist or can't be read, just start with empty history
            }
        }
    }
}

/// Write history to HISTFILE environment variable when exiting
fn write_history_to_file() {
    if let Ok(histfile) = env::var("HISTFILE") {
        let history = HISTORY.lock().unwrap();
        match fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&histfile)
        {
            Ok(mut file) => {
                for entry in history.iter() {
                    if let Err(_) = writeln!(file, "{}", entry) {
                        break;
                    }
                }
            }
            Err(_) => {
                // Silently ignore errors writing to history file
            }
        }
    }
}

fn main() {
    use rustyline::Editor;
    use rustyline::error::ReadlineError;
    use rustyline::config::Builder;
    use rustyline::completion::{Completer, Pair};
    use rustyline::hint::Hinter;
    use rustyline::highlight::Highlighter;
    use rustyline::validate::Validator;
    use rustyline::{Context, Helper};

    // Load history from HISTFILE on startup
    load_history_from_file();

    struct ShellHelper {
        tab_state: Mutex<Option<(String, String, String, Vec<(String, bool)>, bool)>>,
    }

    impl Helper for ShellHelper {}
    impl Hinter for ShellHelper {
        type Hint = String;
    }
    impl Highlighter for ShellHelper {}
    impl Validator for ShellHelper {}

    impl Completer for ShellHelper {
        type Candidate = Pair;

        fn complete(
            &self,
            line: &str,
            pos: usize,
            _ctx: &Context<'_>,
        ) -> rustyline::Result<(usize, Vec<Pair>)> {
            let slice = &line[..pos];

            if !slice.contains(' ') && !slice.is_empty() {
                let partial_cmd = slice;

                let completer_for_cmd = {
                    let completions = COMPLETIONS.lock().unwrap();
                    completions.get(partial_cmd).cloned()
                };

                if let Some(completer_path) = completer_for_cmd {
                    let cmd_parts = vec![partial_cmd.to_string()];
                    if let Some(candidates) = invoke_completer(&completer_path, &cmd_parts, 0, line, pos) {
                        if candidates.is_empty() {
                            return Ok((pos, vec![]));
                        }

                        if candidates.len() == 1 {
                            return Ok((
                                0,
                                vec![Pair {
                                    display: candidates[0].clone(),
                                    replacement: format!("{} ", candidates[0]),
                                }],
                            ));
                        }

                        let mut state = self.tab_state.lock().unwrap();

                        let is_first_tab = if let Some((last_line, _, _, last_matches, _)) = state.as_ref() {
                            let last_matches_names: Vec<String> = last_matches.iter().map(|(n, _)| n.clone()).collect();
                            !(last_line == line && last_matches_names == candidates)
                        } else {
                            true
                        };

                        if is_first_tab {
                            print!("\x07");
                            std::io::stdout().flush().ok();

                            *state = Some((
                                line.to_string(),
                                String::new(),
                                String::new(),
                                candidates.iter().map(|c| (c.clone(), false)).collect(),
                                true,
                            ));

                            return Ok((pos, vec![]));
                        } else {
                            let output = candidates.join("  ");
                            println!();
                            print!("{}", output);
                            println!();
                            print!("$ {}", line);
                            std::io::stdout().flush().ok();

                            *state = Some((
                                line.to_string(),
                                String::new(),
                                String::new(),
                                candidates.iter().map(|c| (c.clone(), false)).collect(),
                                false,
                            ));

                            return Ok((pos, vec![]));
                        }
                    }
                }

                let mut matches = find_executables_in_path_matching(slice);

                let builtins = ["echo", "exit", "type", "pwd", "cd", "complete", "jobs", "history", "declare"];
                for builtin in builtins {
                    if builtin.starts_with(slice) && !matches.contains(&builtin.to_string()) {
                        matches.push(builtin.to_string());
                    }
                }

                matches.sort();

                if matches.is_empty() {
                    *self.tab_state.lock().unwrap() = None;
                    return Ok((pos, vec![]));
                }

                if matches.len() == 1 {
                    let candidate = Pair {
                        display: matches[0].clone(),
                        replacement: format!("{} ", matches[0]),
                    };
                    
                    *self.tab_state.lock().unwrap() = None;
                    
                    return Ok((0, vec![candidate]));
                }

                let lcp = longest_common_prefix(&matches);

                let mut state = self.tab_state.lock().unwrap();
                
                let is_first_tab = if let Some((last_line, _, _, last_matches, _)) = state.as_ref() {
                    let last_matches_names: Vec<String> = last_matches.iter().map(|(n, _)| n.clone()).collect();
                    !(last_line == line && last_matches_names == matches)
                } else {
                    true
                };

                if is_first_tab {
                    *state = Some((line.to_string(), String::new(), String::new(), matches.iter().map(|m| (m.clone(), false)).collect(), true));
                    
                    if lcp.len() > slice.len() {
                        let candidate = Pair {
                            display: lcp.clone(),
                            replacement: lcp,
                        };
                        return Ok((0, vec![candidate]));
                    } else {
                        print!("\x07");
                        std::io::stdout().flush().ok();
                        return Ok((pos, vec![]));
                    }
                } else {
                    let output = matches.join("  ");
                    println!();
                    print!("{}", output);
                    println!();
                    print!("$ {}", line);
                    std::io::stdout().flush().ok();
                    
                    *state = Some((line.to_string(), String::new(), String::new(), matches.iter().map(|m| (m.clone(), false)).collect(), false));
                    
                    return Ok((pos, vec![]));
                }
            } else if let Some(last_space_pos) = slice.rfind(' ') {
                let cmd = slice[..last_space_pos].trim();
                
                let base_cmd = if let Some(space_in_cmd) = cmd.find(' ') {
                    &cmd[..space_in_cmd]
                } else {
                    cmd
                };
                
                let completer_for_cmd = {
                    let completions = COMPLETIONS.lock().unwrap();
                    completions.get(base_cmd).cloned()
                };

                if let Some(completer_path) = completer_for_cmd {
                    let cmd_parts = parse_command_with_quotes(slice);
                    
                    if cmd_parts.is_empty() {
                        return Ok((pos, vec![]));
                    }
                    
                    let word_index = cmd_parts.len() - 1;
                    
                    if let Some(candidates) = invoke_completer(&completer_path, &cmd_parts, word_index, line, pos) {
                        let start_pos = last_space_pos + 1;
                        
                        let current_word = if let Some(w) = cmd_parts.last() {
                            w.as_str()
                        } else {
                            ""
                        };
                        
                        if candidates.is_empty() {
                            return Ok((pos, vec![]));
                        }

                        if candidates.len() == 1 {
                            return Ok((
                                start_pos,
                                vec![Pair {
                                    display: candidates[0].clone(),
                                    replacement: format!("{} ", candidates[0]),
                                }],
                            ));
                        }

                        let lcp = longest_common_prefix(&candidates);
                        let mut state = self.tab_state.lock().unwrap();

                        let is_first_tab = if let Some((last_line, _, _, _, _)) = state.as_ref() {
                            last_line != line
                        } else {
                            true
                        };

                        if is_first_tab {
                            if lcp.len() > current_word.len() {
                                *state = Some((
                                    line.to_string(),
                                    String::new(),
                                    current_word.to_string(),
                                    candidates.iter().map(|c| (c.clone(), false)).collect(),
                                    true,
                                ));

                                return Ok((
                                    start_pos,
                                    vec![Pair {
                                        display: lcp.clone(),
                                        replacement: lcp,
                                    }],
                                ));
                            } else {
                                print!("\x07");
                                std::io::stdout().flush().ok();

                                *state = Some((
                                    line.to_string(),
                                    String::new(),
                                    current_word.to_string(),
                                    candidates.iter().map(|c| (c.clone(), false)).collect(),
                                    true,
                                ));

                                return Ok((pos, vec![]));
                            }
                        } else {
                            let output = candidates.join("  ");
                            println!();
                            print!("{}", output);
                            println!();
                            print!("$ {}", line);
                            std::io::stdout().flush().ok();

                            *state = Some((
                                line.to_string(),
                                String::new(),
                                current_word.to_string(),
                                candidates.iter().map(|c| (c.clone(), false)).collect(),
                                false,
                            ));

                            return Ok((pos, vec![]));
                        }
                    }
                }

                let partial = &slice[last_space_pos + 1..];
                
                let (dir_path, prefix, replacement_base) = if let Some(last_slash_pos) = partial.rfind('/') {
                    let dir = &partial[..=last_slash_pos];
                    let pre = &partial[last_slash_pos + 1..];
                    (dir, pre, dir)
                } else {
                    (".", partial, "")
                };
                
                let matches = if dir_path == "." {
                    find_files_in_current_dir_matching(prefix)
                } else {
                    find_files_in_path_matching(dir_path, prefix)
                };
                
                if matches.is_empty() {
                    *self.tab_state.lock().unwrap() = None;
                    return Ok((pos, vec![]));
                }
                
                if matches.len() == 1 {
                    let (match_name, is_dir) = &matches[0];
                    let suffix = if *is_dir { "/" } else { " " };
                    let completion = format!("{}{}{}", replacement_base, match_name, suffix);

                    *self.tab_state.lock().unwrap() = None;

                    return Ok((
                        last_space_pos + 1,
                        vec![Pair {
                            display: match_name.clone(),
                            replacement: completion,
                        }],
                    ));
                }

                let lcp = longest_common_prefix_files(&matches);
                
                let mut state = self.tab_state.lock().unwrap();
                
                let is_first_tab = if let Some((last_line, last_dir, last_prefix, _last_matches, _was_first)) = state.as_ref() {
                    let same_context = last_line == line && last_dir == dir_path && last_prefix == prefix;
                    if same_context {
                        false
                    } else {
                        true
                    }
                } else {
                    true
                };

                if is_first_tab {
                    if lcp.len() > prefix.len() {
                        let completion = format!("{}{}", replacement_base, lcp);
                        
                        *state = Some((
                            line.to_string(),
                            dir_path.to_string(),
                            prefix.to_string(),
                            matches.clone(),
                            true,
                        ));
                        
                        return Ok((
                            last_space_pos + 1,
                            vec![Pair {
                                display: lcp.clone(),
                                replacement: completion,
                            }],
                        ));
                    } else {
                        print!("\x07");
                        std::io::stdout().flush().ok();
                        
                        *state = Some((
                            line.to_string(),
                            dir_path.to_string(),
                            prefix.to_string(),
                            matches.clone(),
                            true,
                        ));
                        
                        return Ok((pos, vec![]));
                    }
                } else {
                    let formatted_matches: Vec<String> = matches
                        .iter()
                        .map(|(name, is_dir)| {
                            if *is_dir {
                                format!("{}/", name)
                            } else {
                                name.clone()
                            }
                        })
                        .collect();
                    
                    let output = formatted_matches.join("  ");
                    println!();
                    print!("{}", output);
                    println!();
                    print!("$ {}", line);
                    std::io::stdout().flush().ok();
                    
                    *state = Some((
                        line.to_string(),
                        dir_path.to_string(),
                        prefix.to_string(),
                        matches.clone(),
                        false,
                    ));
                    
                    return Ok((pos, vec![]));
                }
            }

            Ok((0, vec![]))
        }
    }

    let config = Builder::new()
        .completion_type(CompletionType::List)
        .auto_add_history(true)
        .build();

    let mut rl = Editor::<ShellHelper, _>::with_config(config).unwrap();
    rl.set_helper(Some(ShellHelper {
        tab_state: Mutex::new(None),
    }));

    loop {
        // Automatically reap finished jobs before displaying prompt
        reap_jobs(true);

        let readline = rl.readline("$ ");
        match readline {
            Ok(line) => {
                let command = line.trim();
                if command.is_empty() {
                    continue;
                }

                let (mut parts, redirection) = parse_with_redirection(command);
                if parts.is_empty() {
                    continue;
                }

                let run_background = if !parts.is_empty() && parts[parts.len() - 1] == "&" {
                    parts.pop();
                    true
                } else {
                    false
                };

                if parts.is_empty() {
                    continue;
                }

                let cmd = &parts[0];

                // Add command to history (before executing)
                {
                    let mut history = HISTORY.lock().unwrap();
                    history.push(command.to_string());
                }

                // Check if command contains pipes FIRST
                if parts.contains(&"|".to_string()) {
                    let pipelines = split_by_pipes(&parts);
                    if let Err(e) = execute_pipeline(pipelines, redirection, run_background, command) {
                        eprintln!("{}", e);
                    }
                } else if cmd == "exit" {
                    write_history_to_file();
                    process::exit(0);
                } else if cmd == "echo" {
                    let args = &parts[1..];
                    let output = args.join(" ");

                    if let Some((stderr_filename, is_append)) = &redirection.stderr_target {
                        let _ = fs::OpenOptions::new()
                            .create(true)
                            .append(*is_append)
                            .write(!is_append)
                            .truncate(!is_append)
                            .open(stderr_filename);
                    }

                    if let Some((filename, is_append)) = &redirection.stdout_target {
                        let result = fs::OpenOptions::new()
                            .create(true)
                            .append(*is_append)
                            .write(!is_append)
                            .truncate(!is_append)
                            .open(filename);
                        match result {
                            Ok(mut file) => {
                                let _ = writeln!(file, "{}", output);
                            }
                            Err(e) => {
                                eprintln!("echo: {}: {}", filename, e);
                            }
                        }
                    } else {
                        println!("{}", output);
                    }
                } else if cmd == "type" {
                    if parts.len() < 2 {
                        println!("type: missing argument");
                        continue;
                    }
                    let target_cmd = &parts[1];
                    if is_builtin(target_cmd) {
                        println!("{} is a shell builtin", target_cmd);
                    } else if let Some(full_path) = find_executable_in_path(target_cmd) {
                        println!("{} is {}", target_cmd, full_path);
                    } else {
                        println!("{}: not found", target_cmd);
                    }
                } else if cmd == "pwd" {
                    match env::current_dir() {
                        Ok(path) => {
                            let output = format!("{}", path.display());
                            if let Some((stderr_filename, is_append)) = &redirection.stderr_target {
                                let _ = fs::OpenOptions::new()
                                    .create(true)
                                    .append(*is_append)
                                    .write(!is_append)
                                    .truncate(!is_append)
                                    .open(stderr_filename);
                            }
                            if let Some((filename, is_append)) = &redirection.stdout_target {
                                let result = fs::OpenOptions::new()
                                    .create(true)
                                    .append(*is_append)
                                    .write(!is_append)
                                    .truncate(!is_append)
                                    .open(filename);
                                match result {
                                    Ok(mut file) => {
                                        let _ = writeln!(file, "{}", output);
                                    }
                                    Err(e) => {
                                        eprintln!("pwd: {}: {}", filename, e);
                                    }
                                }
                            } else {
                                println!("{}", output);
                            }
                        }
                        Err(e) => {
                            eprintln!("pwd: {}", e);
                        }
                    }
                } else if cmd == "cd" {
                    if parts.len() < 2 {
                        eprintln!("cd: missing argument");
                        continue;
                    }
                    let target_dir = &parts[1];
                    let expanded_target = expand_tilde(target_dir);
                    let path = if expanded_target.starts_with('/') {
                        PathBuf::from(&expanded_target)
                    } else {
                        resolve_relative_path(&expanded_target)
                    };

                    if path.exists() && path.is_dir() {
                        if let Err(e) = env::set_current_dir(&path) {
                            eprintln!("cd: {}: {}", target_dir, e);
                        }
                    } else {
                        eprintln!("cd: {}: No such file or directory", target_dir);
                    }
                } else if cmd == "jobs" {
                    // Refresh statuses without prompt printing
                    reap_jobs(false);

                    let mut jobs = JOBS.lock().unwrap();
                    let markers = get_job_markers(&jobs);

                    for job in jobs.iter() {
                        let marker = markers.get(&job.pid).copied().unwrap_or(" ");
                        if job.status == "Done" {
                            println!(
                                "[{}]{}  {:<24}{}",
                                job.job_number, marker, job.status, job.command
                            );
                        } else {
                            println!(
                                "[{}]{}  {:<24}{} &",
                                job.job_number, marker, job.status, job.command
                            );
                        }
                    }

                    // Clear 'Done' jobs after being printed by builtin `jobs`
                    jobs.retain(|job| job.status != "Done");
                } else if cmd == "history" {
                    // Handle history -r <path_to_history_file>
                    if parts.len() > 1 && parts[1] == "-r" {
                        if parts.len() < 3 {
                            eprintln!("history: -r: option requires an argument");
                            continue;
                        }
                        
                        let history_file_path = &parts[2];
                        
                        // Read the history file and append its contents to the history
                        match fs::read_to_string(history_file_path) {
                            Ok(content) => {
                                let mut history = HISTORY.lock().unwrap();
                                for line in content.lines() {
                                    if !line.is_empty() {
                                        history.push(line.to_string());
                                    }
                                }
                                // Update last appended index
                                let mut last_appended = LAST_APPENDED_INDEX.lock().unwrap();
                                *last_appended = history.len();
                            }
                            Err(e) => {
                                eprintln!("history: {}: {}", history_file_path, e);
                            }
                        }
                    } else if parts.len() > 1 && parts[1] == "-w" {
                        // Handle history -w <path_to_history_file>
                        if parts.len() < 3 {
                            eprintln!("history: -w: option requires an argument");
                            continue;
                        }
                        
                        let history_file_path = &parts[2];
                        
                        // Write the history to the file
                        let history = HISTORY.lock().unwrap();
                        match fs::OpenOptions::new()
                            .create(true)
                            .write(true)
                            .truncate(true)
                            .open(history_file_path)
                        {
                            Ok(mut file) => {
                                for entry in history.iter() {
                                    if let Err(e) = writeln!(file, "{}", entry) {
                                        eprintln!("history: {}: {}", history_file_path, e);
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("history: {}: {}", history_file_path, e);
                            }
                        }
                        // Update last appended index
                        let mut last_appended = LAST_APPENDED_INDEX.lock().unwrap();
                        *last_appended = history.len();
                    } else if parts.len() > 1 && parts[1] == "-a" {
                        // Handle history -a <path_to_history_file>
                        if parts.len() < 3 {
                            eprintln!("history: -a: option requires an argument");
                            continue;
                        }
                        
                        let history_file_path = &parts[2];
                        
                        // Append only new commands since last -a
                        let history = HISTORY.lock().unwrap();
                        let mut last_appended = LAST_APPENDED_INDEX.lock().unwrap();
                        
                        let start_index = *last_appended;
                        let end_index = history.len();
                        
                        // Only write if there are new commands
                        if start_index < end_index {
                            match fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(history_file_path)
                            {
                                Ok(mut file) => {
                                    for entry in history[start_index..end_index].iter() {
                                        if let Err(e) = writeln!(file, "{}", entry) {
                                            eprintln!("history: {}: {}", history_file_path, e);
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("history: {}: {}", history_file_path, e);
                                }
                            }
                        }
                        
                        // Update last appended index
                        *last_appended = end_index;
                    } else {
                        // Regular history display
                        let history = HISTORY.lock().unwrap();
                        
                        // Determine how many entries to display
                        let num_to_display = if parts.len() > 1 {
                            // Parse the optional number argument
                            match parts[1].parse::<usize>() {
                                Ok(n) => n,
                                Err(_) => {
                                    eprintln!("history: {}: numeric argument required", parts[1]);
                                    continue;
                                }
                            }
                        } else {
                            // No argument: display all history
                            history.len()
                        };
                        
                        // Calculate starting index
                        let total_entries = history.len();
                        let start_index = if num_to_display >= total_entries {
                            0
                        } else {
                            total_entries - num_to_display
                        };
                        
                        // Display the requested entries
                        for (i, cmd_entry) in history[start_index..].iter().enumerate() {
                            let line_number = start_index + i + 1;
                            println!("{:5}  {}", line_number, cmd_entry);
                        }
                    }
                } else if cmd == "declare" {
                    // Handle declare builtin with -p flag
                    if parts.len() < 2 {
                        eprintln!("declare: missing arguments");
                        continue;
                    }

                    if parts[1] == "-p" {
                        // declare -p flag: print variable declaration
                        if parts.len() < 3 {
                            eprintln!("declare: -p: option requires an argument");
                            continue;
                        }

                        let var_name = &parts[2];
                        
                        // For now, since we don't have a variable store yet,
                        // we always report that the variable is not found
                        eprintln!("declare: {}: not found", var_name);
                    } else {
                        eprintln!("declare: unknown option or missing flag");
                    }
                } else if cmd == "complete" {
                    if parts.len() < 2 {
                        continue;
                    }
                    
                    if parts[1] == "-r" {
                        if parts.len() < 3 {
                            eprintln!("complete: -r: option requires an argument");
                            continue;
                        }
                        
                        let command_name = &parts[2];
                        let mut completions = COMPLETIONS.lock().unwrap();
                        completions.remove(command_name);
                    } else if parts[1] == "-C" {
                        if parts.len() < 4 {
                            eprintln!("complete: -C: option requires an argument");
                            continue;
                        }
                        
                        let completer_path = &parts[2];
                        let command_name = &parts[3];
                        
                        let mut completions = COMPLETIONS.lock().unwrap();
                        completions.insert(command_name.clone(), completer_path.clone());
                    } else if parts[1] == "-p" {
                        if parts.len() < 3 {
                            eprintln!("complete: -p: option requires an argument");
                            continue;
                        }
                        
                        let command_name = &parts[2];
                        let completions = COMPLETIONS.lock().unwrap();
                        
                        if let Some(completer_path) = completions.get(command_name) {
                            println!("complete -C '{}' {}", completer_path, command_name);
                        } else {
                            eprintln!("complete: {}: no completion specification", command_name);
                        }
                    }
                } else {
                    let args = parts[1..].to_vec();
                    if let Err(e) = execute_external_program(cmd, &args, redirection, run_background, command) {
                        eprintln!("{}", e);
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                continue;
            }
            Err(ReadlineError::Eof) => {
                write_history_to_file();
                break;
            }
            Err(err) => {
                eprintln!("Error: {:?}", err);
                write_history_to_file();
                break;
            }
        }
    }
}
