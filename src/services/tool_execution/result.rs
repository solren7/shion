//! The LLM-facing result cap.

/// Truncate an over-long tool result at a UTF-8 char boundary, appending a
/// marker that nudges the model to re-query more narrowly. Short results pass
/// through untouched. Applied uniformly to every tool at the execution choke
/// point, so no individual tool has to implement its own ceiling. A single
/// tool returning tens of KB (a big file read, a full `/api/states` dump, a
/// long web page) would otherwise flood the context window *every subsequent
/// turn*, since the result stays in history. The cap is instance-owned
/// executor config, sized **above** the per-tool self-caps (`web_fetch` /
/// `homeassistant` cap themselves at 8 KB) so it only catches tools that
/// don't self-trim.
pub(super) fn cap_tool_result(mut out: String, cap: usize) -> String {
    if out.len() <= cap {
        return out;
    }
    let mut end = cap;
    while !out.is_char_boundary(end) {
        end -= 1;
    }
    out.truncate(end);
    out.push_str(&format!(
        "\n\n…[truncated: result exceeded the {} KB tool-result limit. Re-run with \
         a narrower query — a filter, a specific id, or a smaller range — to see the rest.]",
        cap / 1024
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_preserves_multibyte_boundaries() {
        // A run of 3-byte CJK chars whose total exceeds the cap: the cut must
        // land on a char boundary, not mid-codepoint (would panic otherwise).
        let big = "界".repeat(4096); // 3 bytes each
        let capped = cap_tool_result(big, 4096);
        assert!(capped.contains("truncated"));
    }

    #[test]
    fn short_results_pass_through() {
        assert_eq!(cap_tool_result("ok".into(), 4096), "ok");
    }
}
