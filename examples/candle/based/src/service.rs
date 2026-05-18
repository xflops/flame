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

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use candle::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::generation::LogitsProcessor;
use candle_transformers::models::based::{Config, Model};
use flame::apis::FlameError;
use flame::service::FlameInstance;
use flame_rs::{self as flame};
use hf_hub::{api::sync::Api, Repo, RepoType};
use tokenizers::Tokenizer;

use self::api::{
    BasedModelOptions, BasedModelSize, GenerateRequest, GenerateResponse, DEFAULT_TEMPERATURE,
    DEFAULT_TOP_P,
};

#[derive(Default)]
struct BasedService {
    // Flame serializes enter/task/leave callbacks for one service instance. The
    // Mutex is still useful because the Rust service macro exposes hooks as
    // `&self`; it provides safe interior mutability for loading and clearing the
    // model without requiring the public hook API to take `&mut self`.
    model: Mutex<Option<ModelBundle>>,
}

#[derive(Clone)]
struct ModelBundle {
    model: Model,
    device: Device,
    tokenizer: Tokenizer,
    eos_token: u32,
}

impl ModelBundle {
    fn generate(mut self, req: GenerateRequest) -> Result<GenerateResponse, FlameError> {
        if req.prompt.trim().is_empty() {
            return Err(FlameError::InvalidConfig(
                "prompt must not be empty".to_string(),
            ));
        }

        let mut tokens = self
            .tokenizer
            .encode(req.prompt.as_str(), true)
            .map_err(internal)?
            .get_ids()
            .to_vec();
        let prompt_tokens = tokens.len();
        if prompt_tokens == 0 {
            return Err(FlameError::InvalidConfig(
                "prompt produced no tokens".to_string(),
            ));
        }

        let temperature = req.temperature.or(Some(DEFAULT_TEMPERATURE));
        let top_p = req.top_p.or(Some(DEFAULT_TOP_P));
        let mut logits_processor = LogitsProcessor::new(req.seed, temperature, top_p);
        let start = Instant::now();
        let mut generated_tokens = 0usize;

        for index in 0..req.sample_len {
            let context_size = if index > 0 { 1 } else { tokens.len() };
            let start_pos = tokens.len().saturating_sub(context_size);
            let context = &tokens[start_pos..];
            let input = Tensor::new(context, &self.device)
                .map_err(internal)?
                .unsqueeze(0)
                .map_err(internal)?;
            let logits = self
                .model
                .forward(&input, start_pos)
                .map_err(internal)?
                .squeeze(0)
                .map_err(internal)?
                .squeeze(0)
                .map_err(internal)?
                .to_dtype(DType::F32)
                .map_err(internal)?;
            let logits = if req.repeat_penalty == 1.0 {
                logits
            } else {
                let start_at = tokens.len().saturating_sub(req.repeat_last_n);
                candle_transformers::utils::apply_repeat_penalty(
                    &logits,
                    req.repeat_penalty,
                    &tokens[start_at..],
                )
                .map_err(internal)?
            };

            let next_token = logits_processor.sample(&logits).map_err(internal)?;
            tokens.push(next_token);
            generated_tokens += 1;
            if next_token == self.eos_token {
                break;
            }
        }

        let elapsed = start.elapsed();
        let text = self.tokenizer.decode(&tokens, true).map_err(internal)?;
        let tokens_per_second = if elapsed.as_secs_f64() > 0.0 {
            generated_tokens as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };

        Ok(GenerateResponse {
            text,
            generated_tokens,
            elapsed_ms: elapsed.as_millis() as u64,
            tokens_per_second,
        })
    }
}

#[flame::instance]
impl BasedService {
    async fn enter(&self, instance: FlameInstance) -> Result<(), FlameError> {
        let options = instance
            .common_data::<BasedModelOptions>()?
            .unwrap_or_default();
        tracing::info!(
            model = %options.model_id.as_deref().unwrap_or(default_model_id(options.which)),
            revision = %options.revision,
            "loading Based model"
        );
        let bundle = load_model(options)?;

        *self.model.lock().map_err(lock_error)? = Some(bundle);
        Ok(())
    }

    #[flame::entrypoint]
    async fn generate(&self, req: GenerateRequest) -> Result<GenerateResponse, FlameError> {
        let bundle = self
            .model
            .lock()
            .map_err(lock_error)?
            .as_ref()
            .cloned()
            .ok_or_else(|| FlameError::InvalidConfig("Based model is not loaded".to_string()))?;
        bundle.generate(req)
    }

    async fn leave(&self) -> Result<(), FlameError> {
        *self.model.lock().map_err(lock_error)? = None;
        Ok(())
    }
}

fn load_model(options: BasedModelOptions) -> Result<ModelBundle, FlameError> {
    let start = Instant::now();
    let api = Api::new().map_err(internal)?;
    let model_id = options
        .model_id
        .clone()
        .unwrap_or_else(|| default_model_id(options.which).to_string());
    let repo = api.repo(Repo::with_revision(
        model_id.clone(),
        RepoType::Model,
        options.revision.clone(),
    ));

    let config_file = match options.config_file {
        Some(path) => PathBuf::from(path),
        None => repo.get("config.json").map_err(internal)?,
    };
    let weight_files = match options.weight_files {
        Some(files) => files.into_iter().map(PathBuf::from).collect(),
        None => vec![repo.get("model.safetensors").map_err(internal)?],
    };

    let tokenizer_repo = api.model(options.tokenizer_id.clone());
    let tokenizer_file = match options.tokenizer_file {
        Some(path) => PathBuf::from(path),
        None => tokenizer_repo.get("tokenizer.json").map_err(internal)?,
    };

    let tokenizer = Tokenizer::from_file(tokenizer_file).map_err(internal)?;
    let eos_token = tokenizer
        .get_vocab(true)
        .get("<|endoftext|>")
        .copied()
        .ok_or_else(|| {
            FlameError::InvalidConfig("tokenizer is missing <|endoftext|>".to_string())
        })?;
    let config_file = std::fs::File::open(config_file).map_err(internal)?;
    let config: Config = serde_json::from_reader(config_file).map_err(internal)?;
    let device = candle_device(options.cpu).map_err(internal)?;
    let dtype = if device.is_cuda() || device.is_metal() {
        DType::BF16
    } else {
        DType::F32
    };
    let mut vb = unsafe { VarBuilder::from_mmaped_safetensors(&weight_files, dtype, &device) }
        .map_err(internal)?;
    if options.which == BasedModelSize::W1b50b {
        vb = vb.pp("model");
    }

    let model = Model::new(&config, vb).map_err(internal)?;

    tracing::info!(
        model = %model_id,
        device = ?device,
        elapsed_ms = start.elapsed().as_millis(),
        "loaded Based model"
    );

    Ok(ModelBundle {
        model,
        device,
        tokenizer,
        eos_token,
    })
}

fn candle_device(cpu: bool) -> candle::Result<Device> {
    if cpu {
        Ok(Device::Cpu)
    } else if candle::utils::cuda_is_available() {
        Device::new_cuda(0)
    } else if candle::utils::metal_is_available() {
        Device::new_metal(0)
    } else {
        Ok(Device::Cpu)
    }
}

fn default_model_id(which: BasedModelSize) -> &'static str {
    match which {
        BasedModelSize::W360m => "hazyresearch/based-360m",
        BasedModelSize::W1b => "hazyresearch/based-1b",
        BasedModelSize::W1b50b => "hazyresearch/based-1b-50b",
    }
}

fn internal(err: impl std::fmt::Display) -> FlameError {
    FlameError::Internal(err.to_string())
}

fn lock_error(_: std::sync::PoisonError<impl Sized>) -> FlameError {
    FlameError::Internal("Based model lock poisoned".to_string())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    flame::run(BasedService::default()).await?;

    tracing::debug!("BasedService was stopped.");

    Ok(())
}
