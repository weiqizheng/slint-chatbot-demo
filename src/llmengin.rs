

#[cfg(feature = "mkl")]
extern crate intel_mkl_src;

#[cfg(feature = "accelerate")]
extern crate accelerate_src;

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use tokenizers::Tokenizer;

use candle_core::quantized::gguf_file;
use candle_core::utils;
use candle_core::{Device, Tensor};
use candle_transformers::generation::LogitsProcessor;
use candle_transformers::models::quantized_llama as quantized_model;

use anyhow::Result;

use super::token_output_stream::TokenOutputStream;

struct Args {
    tokenizer: String,
    model: String,
    sample_len: usize,
    temperature: f64,
    seed: u64,
    repeat_penalty: f32,
    repeat_last_n: usize,
    gqa: usize,
}

impl Args {
    fn tokenizer(&self) -> Result<Tokenizer> {
        let tokenizer_path = PathBuf::from(&self.tokenizer);
        Tokenizer::from_file(tokenizer_path).map_err(anyhow::Error::msg)
    }

    fn model(&self) -> Result<PathBuf> {
        Ok(std::path::PathBuf::from(&self.model))
    }
}

pub fn start_engine(ui_handle: slint::Weak<crate::AppWindow>, receiver: std::sync::mpsc::Receiver<String>) -> anyhow::Result<()> {
    let msg = format!(
        "avx: {}, neon: {}, simd128: {}, f16c: {}",
        utils::with_avx(),
        utils::with_neon(),
        utils::with_simd128(),
        utils::with_f16c()
    );
    super::update_dialog(ui_handle.clone(), msg);

    let args = Args {
        tokenizer: String::from("./tokenizer.json"),
        model: String::from("./openchat-3.5-0106.Q4_K_M.gguf"),
        sample_len: 1000,
        temperature: 0.8,
        seed: 299792458,
        repeat_penalty: 1.1,
        repeat_last_n: 64,
        gqa: 8,
    };

    // device
    // let device = Device::Cpu;
    let device = match Device::new_metal(0) {
        Ok(metal_device) => metal_device,
        Err(err) => {
            println!("metal device init failed {err}");
            Device::Cpu
        },
    };

    // load model
    let model_path = args.model()?;
    let mut file = File::open(&model_path)?;
    let start = std::time::Instant::now();

    // This is the model instance
    let model = gguf_file::Content::read(&mut file)?;
    let mut total_size_in_bytes = 0;
    for (_, tensor) in model.tensor_infos.iter() {
        let elem_count = tensor.shape.elem_count();
        total_size_in_bytes +=
            elem_count * tensor.ggml_dtype.type_size() / tensor.ggml_dtype.block_size();
    }
    let msg = format!(
        "loaded {:?} tensors ({}bytes) in {:.2}s",
        model.tensor_infos.len(),
        total_size_in_bytes,
        start.elapsed().as_secs_f32(),
    );
    super::update_dialog(ui_handle.clone(), msg);
    super::update_dialog(ui_handle.clone(), "loading model...".to_string());
    let mut model = quantized_model::ModelWeights::from_gguf(model, &mut file, &device)?;
    let msg = format!("model built.");
    super::update_dialog(ui_handle.clone(), msg);

    // load tokenizer
    let tokenizer = args.tokenizer()?;
    let mut tos = TokenOutputStream::new(tokenizer);
    // left for future improvement: interactive
    for _prompt_index in 0.. {
        // print!("> ");
        super::update_dialog_without_ln(ui_handle.clone(), "> ".to_string());
        
        let ask = receiver.recv().unwrap();
        if ask == "_exit_" {
            return Err(anyhow::anyhow!("exit".to_string()));
        }
        let prompt_str = format!("User:{ask}<|end_of_turn|>Assistant:");

        // print!("bot: ");
        super::update_dialog_without_ln(ui_handle.clone(), "#Bot: ".to_string());

        let tokens = tos
            .tokenizer()
            .encode(prompt_str, true)
            .map_err(anyhow::Error::msg)?;

        let prompt_tokens = tokens.get_ids();
        let mut all_tokens = vec![];
        let mut logits_processor = LogitsProcessor::new(args.seed, Some(args.temperature), None);

        let start_prompt_processing = std::time::Instant::now();
        let mut next_token = {
            let input = Tensor::new(prompt_tokens, &device)?.unsqueeze(0)?;
            let logits = model.forward(&input, 0)?;
            let logits = logits.squeeze(0)?;
            logits_processor.sample(&logits)?
        };
        let prompt_dt = start_prompt_processing.elapsed();
        all_tokens.push(next_token);
        if let Some(t) = tos.next_token(next_token)? {
            // print!("{t}");
            // std::io::stdout().flush()?;
            super::update_dialog_without_ln(ui_handle.clone(), t);
        }

        let eos_token = "<|end_of_turn|>";
        let eos_token = *tos.tokenizer().get_vocab(true).get(eos_token).unwrap();
        let start_post_prompt = std::time::Instant::now();
        let to_sample = args.sample_len.saturating_sub(1);
        let mut sampled = 0;
        for index in 0..to_sample {
            let input = Tensor::new(&[next_token], &device)?.unsqueeze(0)?;
            let logits = model.forward(&input, prompt_tokens.len() + index)?;
            let logits = logits.squeeze(0)?;
            let logits = if args.repeat_penalty == 1. {
                logits
            } else {
                let start_at = all_tokens.len().saturating_sub(args.repeat_last_n);
                candle_transformers::utils::apply_repeat_penalty(
                    &logits,
                    args.repeat_penalty,
                    &all_tokens[start_at..],
                )?
            };
            next_token = logits_processor.sample(&logits)?;
            all_tokens.push(next_token);
            if let Some(t) = tos.next_token(next_token)? {
                // print!("{t}");
                // std::io::stdout().flush()?;
                super::update_dialog_without_ln(ui_handle.clone(), t);
            }
            sampled += 1;
            if next_token == eos_token {
                break;
            };
        }
        if let Some(rest) = tos.decode_rest().map_err(candle_core::Error::msg)? {
            // print!("{rest}");
            super::update_dialog_without_ln(ui_handle.clone(), rest);
        }

        super::update_dialog(ui_handle.clone(), "".to_string());

        // std::io::stdout().flush()?;
        // let dt = start_post_prompt.elapsed();
        // println!(
        //     "\n\n{:4} prompt tokens processed: {:.2} token/s",
        //     prompt_tokens.len(),
        //     prompt_tokens.len() as f64 / prompt_dt.as_secs_f64(),
        // );
        // println!(
        //     "{sampled:4} tokens generated: {:.2} token/s",
        //     sampled as f64 / dt.as_secs_f64(),
        // );
    }

    Ok(())
}
