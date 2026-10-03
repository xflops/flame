/*
Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

mod api;

use std::error::Error;

use clap::Parser;
use flame_rs::{self as flame, SessionOptions};

use self::api::{
    BasedModelOptions, BasedModelSize, GenerateRequest, GenerateResponse, DEFAULT_REVISION,
    DEFAULT_TEMPERATURE, DEFAULT_TOKENIZER_ID, DEFAULT_TOP_P,
};

const DEFAULT_SAMPLE_LEN: usize = 128;
const DEFAULT_SEED: u64 = 299_792_458;
const DEFAULT_REPEAT_PENALTY: f32 = 1.3;
const DEFAULT_REPEAT_LAST_N: usize = 64;

#[derive(Parser, Debug)]
#[command(name = "candle-based-example")]
#[command(author = "Xflops <support@xflops.io>")]
#[command(version = "0.1.0")]
#[command(about = "Flame Candle Based text generation example", long_about = None)]
struct Cli {
    /// Prompt to complete.
    #[arg(long)]
    prompt: String,
    /// Generated sample length in tokens.
    #[arg(long, short = 'n', default_value_t = DEFAULT_SAMPLE_LEN)]
    sample_len: usize,
    /// Temperature used to generate samples. Use 0 for deterministic argmax.
    #[arg(long, default_value_t = DEFAULT_TEMPERATURE)]
    temperature: f64,
    /// Nucleus sampling probability cutoff. Use 1 to sample from the full distribution.
    #[arg(long, default_value_t = DEFAULT_TOP_P)]
    top_p: f64,
    /// Random seed used when sampling.
    #[arg(long, default_value_t = DEFAULT_SEED)]
    seed: u64,
    /// Penalty applied for repeating tokens, 1.0 means no penalty.
    #[arg(long, default_value_t = DEFAULT_REPEAT_PENALTY)]
    repeat_penalty: f32,
    /// Context size considered for repeat penalty.
    #[arg(long, default_value_t = DEFAULT_REPEAT_LAST_N)]
    repeat_last_n: usize,
    /// Based model variant to run.
    #[arg(long, value_enum, default_value_t = BasedModelSize::default())]
    which: BasedModelSize,
    /// Override the Hugging Face model id.
    #[arg(long)]
    model_id: Option<String>,
    /// Hugging Face model revision.
    #[arg(long, default_value = DEFAULT_REVISION)]
    revision: String,
    /// Hugging Face tokenizer repository.
    #[arg(long, default_value = DEFAULT_TOKENIZER_ID)]
    tokenizer_id: String,
    /// Local config.json path.
    #[arg(long)]
    config_file: Option<String>,
    /// Local tokenizer.json path.
    #[arg(long)]
    tokenizer_file: Option<String>,
    /// Comma-separated local safetensors paths.
    #[arg(long, value_delimiter = ',')]
    weight_files: Vec<String>,
    /// Force CPU execution even when an accelerator is available.
    #[arg(long)]
    cpu: bool,
    /// Flame application name.
    #[arg(long)]
    app: String,
    /// Flame session name. Defaults to an auto-generated name.
    #[arg(long)]
    session: Option<String>,
    /// Minimum service instances to warm for this session.
    #[arg(long, default_value_t = 1)]
    min_instances: u32,
    /// Maximum service instances for this session.
    #[arg(long, default_value_t = 1)]
    max_instances: u32,
    /// Explicit resource request, for example "cpu=4,mem=16g,gpu=1".
    #[arg(short, long)]
    resreq: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    flame::apis::init_logger()?;
    let cli = Cli::parse();

    let model_options = BasedModelOptions {
        which: cli.which,
        model_id: cli.model_id,
        revision: cli.revision,
        tokenizer_id: cli.tokenizer_id,
        config_file: cli.config_file,
        tokenizer_file: cli.tokenizer_file,
        weight_files: if cli.weight_files.is_empty() {
            None
        } else {
            Some(cli.weight_files)
        },
        cpu: cli.cpu,
    };

    let mut options = SessionOptions::new(cli.app)
        .min_instances(cli.min_instances)
        .max_instances(cli.max_instances);
    if let Some(session) = cli.session {
        options = options.name(session);
    }
    if let Some(resreq) = cli.resreq {
        options = options.resreq(resreq);
    }
    options = options.common_data(&model_options)?;

    let session = flame::create_session(options).await?;
    let request = GenerateRequest {
        prompt: cli.prompt,
        sample_len: cli.sample_len,
        temperature: Some(cli.temperature),
        top_p: Some(cli.top_p),
        seed: cli.seed,
        repeat_penalty: cli.repeat_penalty,
        repeat_last_n: cli.repeat_last_n,
    };
    let response: GenerateResponse = session.invoke(&request).await?.await?.ok_or_else(|| {
        flame::apis::FlameError::Internal(
            "candle-based-example-service returned no output".to_string(),
        )
    })?;

    println!("{}", response.text);
    println!(
        "\n{} tokens generated in {} ms ({:.2} token/s)",
        response.generated_tokens, response.elapsed_ms, response.tokens_per_second
    );

    session.close().await?;

    Ok(())
}
