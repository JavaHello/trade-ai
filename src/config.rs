use clap::Parser;

#[derive(Parser, Clone, Debug)]
pub struct CliParams {
    #[clap(short, long, default_value = "BTC-USDT-SWAP")]
    pub inst_id: String,

    /// 价格报警阈值(小于)
    #[clap(short, long, default_value = "-1")]
    pub lower_threshold: f64,

    /// 价格报警阈值(大于)
    #[clap(short, long, default_value = "1000000000000000000")]
    pub upper_threshold: f64,
}
