use std::path::Path;

use brush_core::escape;

#[allow(dead_code)]
pub(crate) async fn complete_async(
    shell: &mut brush_core::Shell<impl brush_core::ShellExtensions>,
    line: &str,
    pos: usize,
) -> brush_core::completion::Completions {
    // Clone the session before the shell is borrowed mutably below; it is an
    // `Arc` and a virtual path, so this is cheap.
    let session = shell.session().clone();

    // Intentionally ignore any errors that arise.
    let completion_future = shell.complete(line, pos);
    tokio::pin!(completion_future);

    // Wait for the completions to come back or interruption, whichever happens first.
    let result = tokio::select! {
        result = &mut completion_future => {
            result
        }
        _ = tokio::signal::ctrl_c() => {
            Err(brush_core::ErrorKind::Interrupted.into())
        },
    };

    let mut completions = result.unwrap_or_else(|_| brush_core::completion::Completions {
        insertion_index: pos,
        delete_count: 0,
        candidates: Vec::new(),
        options: brush_core::completion::ProcessingOptions::default(),
    });

    // Look at the line up to 'pos' to check if we're in an unterminated
    // single or double quote string.
    let mut quote_char: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if i >= pos {
            break;
        }

        if escaped {
            escaped = false;
            continue;
        }

        if let Some(q) = quote_char {
            if c == q {
                quote_char = None;
            }
        } else if c == '\\' {
            escaped = true;
        } else if c == '\'' || c == '\"' {
            quote_char = Some(c);
        }
    }

    let completing_end_of_line = pos == line.len();

    // Deduplicate the candidates (retaining order), then postprocess them.
    completions.candidates = completions
        .candidates
        .into_iter()
        .collect::<indexmap::IndexSet<_>>()
        .into_iter()
        .map(|candidate| {
            postprocess_completion_candidate(
                candidate,
                &completions.options,
                &session,
                completing_end_of_line,
                quote_char,
            )
        })
        .collect();

    completions
}

#[allow(dead_code)]
fn postprocess_completion_candidate(
    mut candidate: String,
    options: &brush_core::completion::ProcessingOptions,
    session: &brush_core::vfs::Session,
    completing_end_of_line: bool,
    quote_char: Option<char>,
) -> String {
    if options.treat_as_filenames {
        // Check if it's a directory. The session resolves a relative candidate
        // against its own working directory, so no joining is needed here.
        if !brush_core::sys::fs::ends_with_path_separator(&candidate) {
            if brush_core::namespace::is_dir(session, Path::new(&candidate)) {
                // Use forward slash: backslash is the shell escape character.
                candidate.push('/');
            }
        }

        if !options.no_autoquote_filenames {
            let quote_mode = match quote_char {
                Some('\'') => escape::QuoteMode::SingleQuote,
                Some('\"') => escape::QuoteMode::DoubleQuote,
                _ => escape::QuoteMode::BackslashEscape,
            };

            candidate = escape::quote_if_needed(&candidate, quote_mode).to_string();
        }
    }
    if completing_end_of_line && !options.no_trailing_space_at_end_of_line {
        if !options.treat_as_filenames || !brush_core::sys::fs::ends_with_path_separator(&candidate)
        {
            candidate.push(' ');
        }
    }

    candidate
}
