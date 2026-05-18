# OpenAI Agent Example

This example demonstrates how to use the [Flame](https://github.com/xflops-io/flame) SDK to deploy an AI agent powered by the [OpenAI Agents SDK](https://github.com/openai/openai-agents-python).

## Requirements

- Python 3.12+
- [FlamePy](https://github.com/xflops-io/flame) (`flamepy`) >= 0.5.0
- [openai](https://pypi.org/project/openai/) >= 2.8.1
- [openai-agents](https://pypi.org/project/openai-agents/) >= 0.6.1

## Files

- `apis.py`: Shared data classes for both the agent and its client.
- `agent.py`: Example agent implementation built with the OpenAI Agents SDK and Flame SDK.
- `client.py`: Client code that communicates with the agent via the Flame API.

## Usage

### 1. Setup Flame Cluster

In the Flame root directory, start a development cluster using `docker compose`:

```bash
docker compose up -d
```

Check the cluster's status:

```bash
docker compose ps
```

Expected output:

```bash
CONTAINER ID  IMAGE                                           COMMAND               CREATED            STATUS            PORTS                   NAMES
dd76fcaa904d  localhost/xflops/flame-session-manager:latest                         About an hour ago  Up About an hour  0.0.0.0:8080->8080/tcp  flame_flame-session-manager_1
b635c3e0fcad  localhost/xflops/flame-executor-manager:latest                        About an hour ago  Up About an hour                          flame_flame-executor-manager_1
4cf654580916  localhost/xflops/flame-console:latest           service ssh start...  About an hour ago  Up About an hour                          flame_flame-console_1
```

Then, log into the console container to run the example:

```bash
docker exec -it flame_flame-console_1 /bin/bash
cd /opt/examples/agents/openai/
```

### 2. Set DeepSeek API Key

Update `openai-agent.yaml` to add your DeepSeek API key, then register the agent in Flame:

```bash
flmctl register -f openai-agent.yaml
```

### 3. Run the Client

```bash
uv run client.py -m "What is the weather like in Seattle?"
```

## Example Output

```bash
# uv run client.py -m "What is the weather like in Seattle?"
Using CPython 3.12.3 interpreter at: /usr/bin/python3.12
Creating virtual environment at: .venv
Downloading setuptools (1.1MiB)
Downloading grpcio (6.1MiB)
Downloading cryptography (4.1MiB)
Downloading grpcio-tools (2.5MiB)
Downloading pydantic-core (1.8MiB)
 Downloading setuptools
 Downloading pydantic-core
 Downloading grpcio-tools
 Downloading cryptography
 Downloading grpcio
warning: Failed to hardlink files; falling back to full copy. This may lead to degraded performance.
         If the cache and target directories are on different filesystems, hardlinking may not be supported.
         If this is intentional, set `export UV_LINK_MODE=copy` or use `--link-mode=copy` to suppress this warning.
Installed 49 packages in 2.20s
Hello! I'm your AI weather forecaster assistant. I'm here to provide you with weather-related information, forecasts, climate insights, and tips to help you plan your day or understand weather patterns. Whether you need current conditions, a forecast for your location, or explanations about meteorological phenomena, I'm here to help! How can I assist you today?
```

## References

- [Flame Documentation](https://github.com/xflops-io/flame)
- [OpenAI Python Library](https://github.com/openai/openai-python)
- [OpenAI Agents SDK](https://github.com/openai/openai-agents-python)

---
