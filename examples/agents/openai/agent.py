# Copyright 2025 The Flame Authors.
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#     http://www.apache.org/licenses/LICENSE-2.0
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

import os
import logging
from typing_extensions import TypedDict

from openai import AsyncOpenAI
from agents import (
    Agent,
    Runner,
    function_tool,
    set_tracing_disabled,
    enable_verbose_stdout_logging,
    set_default_openai_client,
    set_default_openai_api,
)

from flamepy import service
from apis import MyContext, Question, Answer, MyCustomSession

logger = logging.getLogger(__name__)

# Set the default OpenAI client to the DeepSeek client
ds_client = AsyncOpenAI(
    base_url="https://api.deepseek.com", api_key=os.getenv("DEEPSEEK_API_KEY")
)
set_default_openai_client(ds_client)
set_tracing_disabled(True)
enable_verbose_stdout_logging()
set_default_openai_api("chat_completions")

# Creat a FlameInstance
ins = service.FlameInstance()


class Location(TypedDict):
    lat: float
    long: float


@function_tool
async def fetch_weather(location: Location) -> str:
    """Fetch the weather for a given location.

    Args:
        location: The location to fetch the weather for.
    """
    # In real life, we'd fetch the weather from a weather API
    return "sunny"


# Create agent
agent = Agent(
    name="openai-agent-example",
    model="deepseek-chat",
    tools=[fetch_weather],
)


@ins.entrypoint
async def my_agent(q: Question) -> Answer:
    global agent

    ctx = ins.context()

    logger.info(f"ctx: {ctx}, question: {q}")

    if ctx is not None and isinstance(ctx, MyContext):
        session = MyCustomSession(ctx)
        agent.instructions = ctx.prompt

        result = await Runner.run(agent, q.question, session=session)

        ctx.messages = session.history()

        logger.info(f"Update context: {ctx}")
        ins.update_context(ctx)
        logger.info("Update context done")
    else:
        logger.info("Run agent without session")
        result = await Runner.run(agent, q.question)

    return Answer(answer=result.final_output)


if __name__ == "__main__":
    logging.basicConfig(level=logging.DEBUG)
    ins.run()
