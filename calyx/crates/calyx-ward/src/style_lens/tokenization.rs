use crate::error::WardError;

use super::STYLE_MAX_TOKENS;

pub(super) fn model_inputs(
    token_ids: &[u32],
    attention_mask: &[u32],
) -> Result<(Vec<i64>, Vec<i64>), WardError> {
    if token_ids.len() != attention_mask.len() {
        return Err(WardError::InvalidInput {
            reason: format!(
                "style tokenizer emitted {} token IDs but {} attention values",
                token_ids.len(),
                attention_mask.len()
            ),
        });
    }

    let len = token_ids.len();
    if len == 0 {
        return Err(WardError::InvalidInput {
            reason: "style tokenizer emitted no tokens".to_string(),
        });
    }
    if len > STYLE_MAX_TOKENS {
        return Err(WardError::InvalidInput {
            reason: format!(
                "style input encoded to {len} tokens, exceeding the complete-coverage limit of \
                {STYLE_MAX_TOKENS}; reject or shorten the input until its complete encoded \
                 sequence is within the limit"
            ),
        });
    }

    let ids = token_ids
        .iter()
        .map(|value| i64::from(*value))
        .collect::<Vec<_>>();
    let attention = attention_mask
        .iter()
        .map(|value| i64::from(*value))
        .collect::<Vec<_>>();
    Ok((ids, attention))
}
