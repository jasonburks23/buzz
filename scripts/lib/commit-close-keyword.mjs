/**
 * scripts/lib/commit-close-keyword.mjs
 *
 * Pure classifier for closing-keyword patterns in commit messages.
 * Used by the .githooks/commit-msg hook (Piece 3, opeff#350 G2).
 *
 * GitHub treats "Closes #N", "Fixes #N", "Resolves #N" (and variants) as
 * auto-close directives on the default branch. Scope qualifiers appended
 * after the number (e.g. "Closes #350 runner-up") are silently ignored by
 * GitHub: the whole issue is closed. This classifier catches that footgun.
 *
 * Exports:
 *   classifyCommitCloseKeyword(message) -> { block, warn, matches }
 *   SCOPE_QUALIFIERS (Set of canonical qualifier words)
 */

'use strict';

// Closing-keyword regex: matches "close/closes/closed", "fix/fixes/fixed",
// "resolve/resolves/resolved" followed by an issue reference.
//
// opeff#525: the original pattern used `\s+#?(\d+)` -- whitespace ONLY between
// the keyword and the number. GitHub's own parser is wider on one axis and
// narrower on another, and the control must match GitHub, not a superset or a
// subset of it. The BAR is "at least as permissive as GitHub, and no more".
//
//   WIDER than the old regex (GitHub honors these, we missed them -> now caught):
//     - the colon separator: "close: #512" (this closed opeff#512 on cf2652d)
//     - the full issue-URL form: "closes https://github.com/owner/repo/issues/512"
//
//   NARROWER than the old regex (GitHub does NOT honor this, we fired -> now clean):
//     - the bare-number form "fix 99": GitHub does not auto-close on a number
//       with no `#` and no URL. Matching it cries wolf on prose like "fixed 3
//       bugs", and a control that false-positives constantly gets bypassed --
//       a bypassed control is a dead control, which is the failure class this
//       ticket exists to fix (OW ruling + Holdout parity bar, opeff#525).
//
// So: separator is `[\s:]+` (whitespace and/or colon), and the target REQUIRES
// either the issues-URL prefix or a literal `#` (the `#`/URL is no longer
// optional). The `\b` after the keyword still prevents "closes5"-style runs.
//
// KNOWN RESIDUAL (documented, not a silent gap): the cross-repo shorthand
// "closes owner/repo#5" and the "GH-5" style are not matched. They are rarer
// close vectors; the colon and URL forms are the two that fired tonight.
const CLOSE_KEYWORD_RE =
  /\b(clos(?:e|es|ed)|fix(?:e|es|ed)?|resolv(?:e|es|ed))\b[\s:]+(?:https?:\/\/github\.com\/[^\s/]+\/[^\s/]+\/issues\/|#)(\d+)/gi;

/**
 * Qualifier words that, when appended after the issue number, indicate a
 * sub-part intent. GitHub ignores them; the hook blocks the commit and
 * instructs the author to use "part of #N" instead.
 *
 * Includes hyphenated variants and common shortenings.
 */
export const SCOPE_QUALIFIERS = new Set([
  'runner-up',
  'runnerup',
  'part',
  'subpart',
  'sub-part',
  'subset',
  'sub',
  'piece',
  'portion',
  'half',
  'item',
  'sub-deliverable',
  'subdeliverable',
]);

/**
 * classifyCommitCloseKeyword(message)
 *
 * Pure function. No I/O. Safe to unit-test with injected strings.
 *
 * Scans the commit message for closing-keyword patterns and classifies them:
 *   - block: a qualifier word follows the issue number (footgun: GitHub still
 *            closes the whole issue despite the qualifier).
 *   - warn:  a bare closing keyword targets an issue number (will auto-close
 *            on default branch; not necessarily wrong, but worth confirming).
 *
 * @param {string} message - full commit message text
 * @returns {{ block: boolean, warn: boolean, matches: Array<{keyword: string, issue: string, qualifier: string|null}> }}
 */
export function classifyCommitCloseKeyword(message) {
  if (typeof message !== 'string' || message.length === 0) {
    return { block: false, warn: false, matches: [] };
  }

  const matches = [];
  let block = false;
  let warn = false;

  let m;
  CLOSE_KEYWORD_RE.lastIndex = 0;

  while ((m = CLOSE_KEYWORD_RE.exec(message)) !== null) {
    const keyword = m[1];
    const issue = m[2];

    // Look at the token immediately following the issue number.
    // Slice from just after the full match and grab the next whitespace-separated word.
    const afterMatch = message.slice(m.index + m[0].length);
    const nextTokenMatch = afterMatch.match(/^\s+([A-Za-z][A-Za-z0-9_-]*)/);
    const nextToken = nextTokenMatch ? nextTokenMatch[1].toLowerCase() : null;

    const isQualifier = nextToken !== null && SCOPE_QUALIFIERS.has(nextToken);

    if (isQualifier) {
      block = true;
      matches.push({ keyword, issue, qualifier: nextToken });
    } else {
      warn = true;
      matches.push({ keyword, issue, qualifier: null });
    }
  }

  // block wins over warn for overall behavior: the hook checks block first,
  // then warn only when not block. Both flags are returned as-is so callers
  // can inspect the raw state; the hook applies the precedence in its own logic.
  return { block, warn, matches };
}
