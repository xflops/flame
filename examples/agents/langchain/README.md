# LangChain Agent Example

This example runs a LangChain-powered Flame service backed by DeepSeek's OpenAI-compatible chat API.

## Run

From the Flame console:

```bash
cd /opt/examples/agents/langchain
flmctl register -f langchain-agent.yaml
uv run single_ssn.py
uv run multi_ssn.py
```

Set `DEEPSEEK_API_KEY` in `langchain-agent.yaml` before registering the application.

## Files

- `agent.py`: Flame service entrypoint that invokes a LangChain agent.
- `single_ssn.py`: Sends one request with one session context.
- `multi_ssn.py`: Shows separate sessions with different system prompts.
- `apis.py`: Shared request, response, and context dataclasses.
- `langchain-agent.yaml`: Application manifest.
