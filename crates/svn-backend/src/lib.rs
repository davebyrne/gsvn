//! Subversion access for gsvn.
//!
//! Runs the `svn` command-line client with `--xml --non-interactive` and
//! parses its output. Has no GTK dependency so it can be tested against
//! recorded XML fixtures. See DESIGN.md §3.
