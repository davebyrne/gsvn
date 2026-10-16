use super::ParseError;
use crate::model::*;
use std::io::BufRead;

pub(super) fn parse<R: BufRead>(_r: R, _emit: impl FnMut(FileDiff)) -> Result<(), ParseError> {
    todo!("parse::diff")
}
