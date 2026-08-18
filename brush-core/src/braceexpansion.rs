use brush_parser::word;
use itertools::Itertools;

use crate::error;

/// Maximum number of fields one brace expansion may produce.
///
/// bash imposes no limit and will exhaust memory on `echo {1..1000000000}`.
/// The cap has to bound both halves of the expansion: a single member sequence,
/// and the cartesian product across pieces -- `{a,b}` repeated thirty times is a
/// billion fields from sixty characters of input. It is set well beyond any
/// plausible script so that the divergence from bash is only ever reached by
/// something that would have exhausted memory anyway.
const MAX_BRACE_EXPANSION_FIELDS: usize = 1 << 20;

pub(crate) fn generate_and_combine_brace_expansions(
    pieces: Vec<brush_parser::word::BraceExpressionOrText>,
) -> Result<impl IntoIterator<Item = String>, error::Error> {
    let mut running_total: usize = 1;
    let mut expansions: Vec<Vec<String>> = Vec::with_capacity(pieces.len());

    for piece in pieces {
        // Check the size before generating anything. A sequence's length is
        // arithmetic, so `{1..1000000000}` is rejected without materializing a
        // single string -- materializing up to the cap first would make
        // rejection cost as much as the cap allows.
        let (count, expansion) = expand_brace_expr_or_text(piece)?;
        if count > MAX_BRACE_EXPANSION_FIELDS {
            return Err(error::ErrorKind::TooMuchData.into());
        }

        // The product grows multiplicatively: `{a,b}` repeated thirty times is a
        // billion fields from sixty characters of input, with every piece well
        // under the cap on its own.
        running_total = running_total.saturating_mul(count);
        if running_total > MAX_BRACE_EXPANSION_FIELDS {
            return Err(error::ErrorKind::TooMuchData.into());
        }

        expansions.push(expansion.collect());
    }

    Ok(expansions
        .into_iter()
        .multi_cartesian_product()
        .map(|v| v.join("")))
}

/// Expands one piece, returning how many fields it will yield alongside the
/// (still lazy) iterator that yields them. The count is exact for sequences and
/// is what lets an oversized expansion be refused before any allocation.
fn expand_brace_expr_or_text(
    beot: word::BraceExpressionOrText,
) -> Result<(usize, Box<dyn Iterator<Item = String>>), error::Error> {
    match beot {
        word::BraceExpressionOrText::Expr(members) => {
            // Chain all member iterators together
            let members = members
                .into_iter()
                .map(expand_brace_expr_member)
                .collect::<Result<Vec<_>, _>>()?;
            let count = members
                .iter()
                .fold(0usize, |acc, (n, _)| acc.saturating_add(*n));
            Ok((count, Box::new(members.into_iter().flat_map(|(_, it)| it))))
        }
        word::BraceExpressionOrText::Text(text) => Ok((1, Box::new(std::iter::once(text)))),
    }
}

/// Number of steps in an inclusive sequence, computed rather than counted.
/// Widened to `i128` because `end - start` overflows `i64` at the extremes.
fn sequence_len(start: i128, end: i128, increment: usize) -> usize {
    let span = (end - start).unsigned_abs();
    let increment = increment as u128;
    usize::try_from(span / increment + 1).unwrap_or(usize::MAX)
}

#[expect(clippy::cast_possible_truncation)]
fn expand_brace_expr_member(
    bem: word::BraceExpressionMember,
) -> Result<(usize, Box<dyn Iterator<Item = String>>), error::Error> {
    match bem {
        word::BraceExpressionMember::NumberSequence {
            start,
            end,
            increment,
        } => {
            let mut increment = increment.unsigned_abs() as usize;
            if increment == 0 {
                increment = 1;
            }

            let count = sequence_len(i128::from(start), i128::from(end), increment);

            if start <= end {
                Ok((
                    count,
                    Box::new((start..=end).step_by(increment).map(|n| n.to_string())),
                ))
            } else {
                // Iterate from start down to end by decrementing.
                #[allow(clippy::cast_possible_wrap)]
                let increment = increment as i64;
                Ok((
                    count,
                    Box::new(
                        std::iter::successors(Some(start), move |&n| {
                            let next = n - increment;
                            (next >= end).then_some(next)
                        })
                        .map(|n| n.to_string()),
                    ),
                ))
            }
        }

        word::BraceExpressionMember::CharSequence {
            start,
            end,
            increment,
        } => {
            let mut increment = increment.unsigned_abs() as usize;
            if increment == 0 {
                increment = 1;
            }

            let count = sequence_len(
                i128::from(u32::from(start)),
                i128::from(u32::from(end)),
                increment,
            );

            if start <= end {
                Ok((
                    count,
                    Box::new((start..=end).step_by(increment).map(|c| c.to_string())),
                ))
            } else {
                // Iterate from start down to end by decrementing.
                let increment = increment as u32;
                Ok((
                    count,
                    Box::new(
                        std::iter::successors(Some(start), move |&c| {
                            let next = char::from_u32(c as u32 - increment)?;
                            (next >= end).then_some(next)
                        })
                        .map(|c| c.to_string()),
                    ),
                ))
            }
        }

        word::BraceExpressionMember::Child(elements) => {
            // Chain all element iterators together. The child's own expansion
            // is already bounded by the same cap, so collecting it here to learn
            // its length cannot exceed the ceiling.
            let child: Vec<String> = generate_and_combine_brace_expansions(elements)?
                .into_iter()
                .collect();
            Ok((child.len(), Box::new(child.into_iter())))
        }
    }
}
