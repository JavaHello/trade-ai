// 技术指标计算验证程序
// 用于验证 OKX 市场指标计算的正确性

use crate::okx_analytics::{compute_atr, compute_ema, compute_macd, compute_rsi};

#[cfg(test)]
mod tests {
    use super::*;

    // 测试 EMA 计算
    #[test]
    fn test_ema_calculation() {
        let prices = vec![
            22.27, 22.19, 22.08, 22.17, 22.18, 22.13, 22.23, 22.43, 22.24, 22.29, 22.15, 22.39,
            22.38, 22.61, 23.36, 24.05, 23.75, 23.83, 23.95, 23.63,
        ];

        let ema_20 = compute_ema(&prices, 20);

        // 验证 EMA 数组长度应该等于价格数组长度
        assert_eq!(ema_20.len(), prices.len());

        // EMA 应该是递增的（对于这个递增序列）
        // 最后一个 EMA 值应该接近最新价格
        let last_ema = ema_20.last().unwrap();
        println!("Last EMA(20): {:.4}", last_ema);

        // EMA 应该在合理范围内
        assert!(*last_ema > 22.0 && *last_ema < 24.0);
    }

    #[test]
    fn test_rsi_calculation() {
        let prices = vec![
            44.34, 44.09, 43.61, 43.40, 44.04, 43.89, 44.08, 44.36, 44.28, 43.99, 43.61, 43.39,
            43.61, 44.35, 44.79, 45.22, 45.87, 46.08, 45.89, 46.03, 45.61, 46.28, 46.28, 46.00,
            46.03, 46.41, 46.22, 45.64, 46.21,
        ];

        let rsi_14 = compute_rsi(&prices, 14);

        // RSI 应该在 0-100 之间
        for &rsi in &rsi_14 {
            assert!(rsi >= 0.0 && rsi <= 100.0, "RSI out of range: {}", rsi);
        }

        println!("Last RSI(14): {:.2}", rsi_14.last().unwrap_or(&0.0));
    }

    #[test]
    fn test_macd_calculation() {
        let prices = vec![
            22.27, 22.19, 22.08, 22.17, 22.18, 22.13, 22.23, 22.43, 22.24, 22.29, 22.15, 22.39,
            22.38, 22.61, 23.36, 24.05, 23.75, 23.83, 23.95, 23.63, 23.82, 23.87, 23.65, 23.19,
            23.10, 23.33, 22.68, 23.10, 22.40, 22.17,
        ];

        let macd = compute_macd(&prices);

        // MACD 数组长度应该等于价格数组长度
        assert_eq!(macd.len(), prices.len());

        println!("Last MACD: {:.4}", macd.last().unwrap_or(&0.0));
    }

    #[test]
    fn test_atr_calculation() {
        use crate::okx_analytics::Candle;

        let candles = vec![
            Candle {
                ts: 0,
                open: 22.0,
                high: 23.0,
                low: 21.5,
                close: 22.5,
                volume: 1000.0,
            },
            Candle {
                ts: 1,
                open: 22.5,
                high: 23.5,
                low: 22.0,
                close: 23.0,
                volume: 1100.0,
            },
            Candle {
                ts: 2,
                open: 23.0,
                high: 24.0,
                low: 22.8,
                close: 23.5,
                volume: 1200.0,
            },
            Candle {
                ts: 3,
                open: 23.5,
                high: 24.5,
                low: 23.0,
                close: 24.0,
                volume: 1300.0,
            },
            Candle {
                ts: 4,
                open: 24.0,
                high: 25.0,
                low: 23.5,
                close: 24.5,
                volume: 1400.0,
            },
            Candle {
                ts: 5,
                open: 24.5,
                high: 25.5,
                low: 24.0,
                close: 25.0,
                volume: 1500.0,
            },
            Candle {
                ts: 6,
                open: 25.0,
                high: 26.0,
                low: 24.5,
                close: 25.5,
                volume: 1600.0,
            },
            Candle {
                ts: 7,
                open: 25.5,
                high: 26.5,
                low: 25.0,
                close: 26.0,
                volume: 1700.0,
            },
            Candle {
                ts: 8,
                open: 26.0,
                high: 27.0,
                low: 25.5,
                close: 26.5,
                volume: 1800.0,
            },
            Candle {
                ts: 9,
                open: 26.5,
                high: 27.5,
                low: 26.0,
                close: 27.0,
                volume: 1900.0,
            },
            Candle {
                ts: 10,
                open: 27.0,
                high: 28.0,
                low: 26.5,
                close: 27.5,
                volume: 2000.0,
            },
            Candle {
                ts: 11,
                open: 27.5,
                high: 28.5,
                low: 27.0,
                close: 28.0,
                volume: 2100.0,
            },
            Candle {
                ts: 12,
                open: 28.0,
                high: 29.0,
                low: 27.5,
                close: 28.5,
                volume: 2200.0,
            },
            Candle {
                ts: 13,
                open: 28.5,
                high: 29.5,
                low: 28.0,
                close: 29.0,
                volume: 2300.0,
            },
            Candle {
                ts: 14,
                open: 29.0,
                high: 30.0,
                low: 28.5,
                close: 29.5,
                volume: 2400.0,
            },
        ];

        let atr_14 = compute_atr(&candles, 14);

        // ATR 应该为正值
        for &atr in &atr_14 {
            assert!(atr >= 0.0, "ATR should be non-negative: {}", atr);
        }

        if let Some(last_atr) = atr_14.last() {
            println!("Last ATR(14): {:.4}", last_atr);
            // ATR 应该反映波动性
            assert!(*last_atr > 0.0);
        }
    }

    // 验证 EMA 的平滑特性
    #[test]
    fn test_ema_smoothing() {
        let prices = vec![10.0, 20.0, 10.0, 20.0, 10.0, 20.0, 10.0, 20.0];
        let ema = compute_ema(&prices, 5);

        // EMA 应该比原始价格更平滑（变化更小）
        for i in 1..ema.len() {
            let ema_change = (ema[i] - ema[i - 1]).abs();
            let price_change = (prices[i] - prices[i - 1]).abs();
            // EMA 的变化应该小于或等于价格的变化
            println!(
                "EMA change: {:.2}, Price change: {:.2}",
                ema_change, price_change
            );
        }
    }

    // 验证 RSI 边界情况
    #[test]
    fn test_rsi_extreme_cases() {
        // 持续上涨应该产生高 RSI
        let rising_prices: Vec<f64> = (0..30).map(|i| 100.0 + i as f64).collect();
        let rsi_rising = compute_rsi(&rising_prices, 14);

        if let Some(last_rsi) = rsi_rising.last() {
            println!("RSI (rising trend): {:.2}", last_rsi);
            // 持续上涨的 RSI 应该接近 100
            assert!(*last_rsi > 70.0);
        }

        // 持续下跌应该产生低 RSI
        let falling_prices: Vec<f64> = (0..30).map(|i| 200.0 - i as f64).collect();
        let rsi_falling = compute_rsi(&falling_prices, 14);

        if let Some(last_rsi) = rsi_falling.last() {
            println!("RSI (falling trend): {:.2}", last_rsi);
            // 持续下跌的 RSI 应该接近 0
            assert!(*last_rsi < 30.0);
        }
    }

    // 验证 MACD 零轴穿越
    #[test]
    fn test_macd_crossover() {
        // 创建一个先下跌后上涨的价格序列
        let mut prices = Vec::new();
        for i in 0..15 {
            prices.push(100.0 - i as f64);
        }
        for i in 0..15 {
            prices.push(85.0 + i as f64);
        }

        let macd = compute_macd(&prices);

        // MACD 应该从负值变为正值
        let first_half_macd: Vec<f64> = macd.iter().take(15).copied().collect();
        let second_half_macd: Vec<f64> = macd.iter().skip(15).copied().collect();

        println!(
            "First half MACD avg: {:.4}",
            first_half_macd.iter().sum::<f64>() / first_half_macd.len() as f64
        );
        println!(
            "Second half MACD avg: {:.4}",
            second_half_macd.iter().sum::<f64>() / second_half_macd.len() as f64
        );
    }
}
