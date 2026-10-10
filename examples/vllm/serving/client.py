"""Submit a batch to the vLLM Flame serving application."""

import argparse
import json

from flamepy import serving


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", default="vllm-serving")
    parser.add_argument("--prompt", action="append", help="Repeat to batch prompts")
    parser.add_argument("--max-tokens", type=int, default=32)
    parser.add_argument("--temperature", type=float, default=0.8)
    args = parser.parse_args()
    context = {
        "max_tokens": args.max_tokens,
        "temperature": args.temperature,
    }
    with serving.open_session(args.app, ctx=context, resreq={"gpu": 1}) as session:
        result = session.run(
            {
                "prompts": args.prompt or ["The future of distributed inference is"],
            }
        )
        print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
