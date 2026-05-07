//! Lenient string→enum parsers for tool arguments.
//!
//! LLMs hand us strings like "buy" / "BUY" / "Buy"; skill-trading's
//! enums are case-sensitive. Centralize the lowercasing + match here
//! instead of repeating it in every tool.

use skill_trading::models::{MarginMode, OrderSide, PositionSide, TimeInForce, TriggerType};

use crate::error::TtcToolError;

/// Strip non-alphanumeric characters and uppercase. The TTC backend and
/// most exchange APIs expect compact symbols like "BTCUSDT", never
/// "BTC-USDT" / "BTC/USDT" / "btc_usdt". LLMs frequently hand us the
/// punctuated form from documentation; normalize at the boundary so every
/// downstream call sees the canonical shape.
pub(crate) fn normalize_symbol(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase()
}

pub(crate) fn parse_side(s: &str) -> Result<OrderSide, TtcToolError> {
    match s.trim().to_lowercase().as_str() {
        "buy" => Ok(OrderSide::Buy),
        "sell" => Ok(OrderSide::Sell),
        _ => Err(TtcToolError::InvalidArg(format!(
            "side must be \"buy\" or \"sell\", got {s:?}"
        ))),
    }
}

pub(crate) fn parse_position_side(s: &str) -> Result<PositionSide, TtcToolError> {
    match s.trim().to_lowercase().as_str() {
        "long" => Ok(PositionSide::Long),
        "short" => Ok(PositionSide::Short),
        "both" => Ok(PositionSide::Both),
        _ => Err(TtcToolError::InvalidArg(format!(
            "position_side must be \"long\", \"short\", or \"both\", got {s:?}"
        ))),
    }
}

pub(crate) fn parse_time_in_force(s: &str) -> Result<TimeInForce, TtcToolError> {
    match s.trim().to_lowercase().as_str() {
        "goodtillcancel" | "gtc" => Ok(TimeInForce::GoodTillCancel),
        "immediateorcancel" | "ioc" => Ok(TimeInForce::ImmediateOrCancel),
        "fillorkill" | "fok" => Ok(TimeInForce::FillOrKill),
        "postonly" => Ok(TimeInForce::PostOnly),
        _ => Err(TtcToolError::InvalidArg(format!(
            "time_in_force must be GoodTillCancel/ImmediateOrCancel/FillOrKill/PostOnly (or GTC/IOC/FOK), got {s:?}"
        ))),
    }
}

pub(crate) fn parse_trigger_type(s: &str) -> Result<TriggerType, TtcToolError> {
    match s.trim().to_lowercase().as_str() {
        "bylastprice" | "last" => Ok(TriggerType::ByLastPrice),
        "bymarkprice" | "mark" => Ok(TriggerType::ByMarkPrice),
        "byindexprice" | "index" => Ok(TriggerType::ByIndexPrice),
        _ => Err(TtcToolError::InvalidArg(format!(
            "trigger_type must be ByLastPrice/ByMarkPrice/ByIndexPrice, got {s:?}"
        ))),
    }
}

pub(crate) fn parse_margin_mode(s: &str) -> Result<MarginMode, TtcToolError> {
    match s.trim().to_lowercase().as_str() {
        "isolated" => Ok(MarginMode::Isolated),
        "cross" => Ok(MarginMode::Cross),
        _ => Err(TtcToolError::InvalidArg(format!(
            "margin_mode must be \"isolated\" or \"cross\", got {s:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_side_accepts_case_insensitive() {
        assert!(matches!(parse_side("buy"), Ok(OrderSide::Buy)));
        assert!(matches!(parse_side("BUY"), Ok(OrderSide::Buy)));
        assert!(matches!(parse_side("Buy"), Ok(OrderSide::Buy)));
        assert!(matches!(parse_side(" sell "), Ok(OrderSide::Sell)));
    }

    #[test]
    fn parse_side_rejects_garbage() {
        let err = parse_side("hodl").unwrap_err();
        assert!(matches!(err, TtcToolError::InvalidArg(_)));
    }

    #[test]
    fn parse_position_side_all_variants() {
        assert!(matches!(parse_position_side("long"), Ok(PositionSide::Long)));
        assert!(matches!(
            parse_position_side("SHORT"),
            Ok(PositionSide::Short)
        ));
        assert!(matches!(parse_position_side("Both"), Ok(PositionSide::Both)));
        assert!(parse_position_side("middle").is_err());
    }

    #[test]
    fn parse_time_in_force_accepts_short_codes() {
        assert!(matches!(
            parse_time_in_force("gtc"),
            Ok(TimeInForce::GoodTillCancel)
        ));
        assert!(matches!(
            parse_time_in_force("IOC"),
            Ok(TimeInForce::ImmediateOrCancel)
        ));
        assert!(matches!(
            parse_time_in_force("fok"),
            Ok(TimeInForce::FillOrKill)
        ));
        assert!(matches!(
            parse_time_in_force("PostOnly"),
            Ok(TimeInForce::PostOnly)
        ));
    }

    #[test]
    fn parse_trigger_type_accepts_short_codes() {
        assert!(matches!(
            parse_trigger_type("last"),
            Ok(TriggerType::ByLastPrice)
        ));
        assert!(matches!(
            parse_trigger_type("BYMARKPRICE"),
            Ok(TriggerType::ByMarkPrice)
        ));
        assert!(parse_trigger_type("by-something-else").is_err());
    }

    #[test]
    fn parse_margin_mode_strict() {
        assert!(matches!(parse_margin_mode("isolated"), Ok(MarginMode::Isolated)));
        assert!(matches!(parse_margin_mode("CROSS"), Ok(MarginMode::Cross)));
        assert!(parse_margin_mode("hybrid").is_err());
    }

    #[test]
    fn normalize_symbol_strips_separators_and_uppercases() {
        assert_eq!(normalize_symbol("BTC-USDT"), "BTCUSDT");
        assert_eq!(normalize_symbol("btc-usdt"), "BTCUSDT");
        assert_eq!(normalize_symbol("BTC/USDT"), "BTCUSDT");
        assert_eq!(normalize_symbol("btc_usdt"), "BTCUSDT");
        assert_eq!(normalize_symbol("BTC.USDT"), "BTCUSDT");
        assert_eq!(normalize_symbol("BTCUSDT"), "BTCUSDT");
        assert_eq!(normalize_symbol(" BTC USDT "), "BTCUSDT");
        assert_eq!(normalize_symbol("OIL/USDT"), "OILUSDT");
        assert_eq!(normalize_symbol("nas100usdt"), "NAS100USDT");
        assert_eq!(normalize_symbol("NAS100-USDT"), "NAS100USDT");
    }
}
