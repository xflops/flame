import flamepy
from flamepy import service
import os
import qdrant_client
from qdrant_client.models import VectorParams, Distance
import logging
from concurrent.futures import wait

from openai import AsyncOpenAI
from agents import (
    Agent,
    Runner,
    function_tool,
    enable_verbose_stdout_logging,
    set_tracing_disabled,
    set_default_openai_client,
    set_default_openai_api,
)

from langchain_community.utilities import DuckDuckGoSearchAPIWrapper
from langchain_community.tools import DuckDuckGoSearchResults

from apis import Question, Answer, Script, WebPage
from embed import EmbeddingClient

logger = logging.getLogger("sra.openai")
logger.setLevel(logging.DEBUG)

script_runner = None
web_crawler = None

# Set the default OpenAI client to the DeepSeek client
ds_client = AsyncOpenAI(
    base_url="https://api.deepseek.com", api_key=os.getenv("DEEPSEEK_API_KEY")
)
set_default_openai_client(ds_client)
set_tracing_disabled(True)
enable_verbose_stdout_logging()
set_default_openai_api("chat_completions")


@function_tool
async def run_script(code: str) -> str:
    """
    Run the python script and return the result. The stdout of the script will be returned as a string.
    The script will be launched by `uv run` command with the dependencies declared in the script.
    For example, if the script depends on `numpy`, you should declare the dependencies in the script like this:
    ```
    # /// script
    # dependencies = [
    #   "numpy",
    # ]
    # ///
    ```
    Reference to https://docs.astral.sh/uv/guides/scripts/ for more details about how to declare the dependencies.

    Args:
        code: the python code to run

    Returns:
        str: the stdout of the script
    """
    global script_runner
    if script_runner is None:
        script_runner = flamepy.create_session("flmexec")

    output = script_runner.invoke(Script(language="python", code=code))

    return output.decode("utf-8")


class Counter(flamepy.TaskInformer):
    """
    Count the number of failed, succeed and error tasks.
    """

    def __init__(self):
        super().__init__()
        self.failed = 0
        self.succeed = 0
        self.error = 0

    def on_update(self, task: flamepy.Task):
        if task.is_failed():
            self.failed += 1
        elif task.is_completed():
            self.succeed += 1

    def on_error(self, _: flamepy.FlameError):
        self.error += 1


@function_tool
async def web_search(topics: list[str]) -> int:
    """
    Search the web for the topics and persist the content of the web page to the vector database.
    Return the number of urls crawled successfully.

    Args:
        topics: the topics to search the web for

    Returns:
        int: the number of urls crawled successfully
    """

    try:
        global web_crawler
        if web_crawler is None:
            web_crawler = flamepy.create_session("crawler")

        wrapper = DuckDuckGoSearchAPIWrapper(time="d", max_results=20)
        search = DuckDuckGoSearchResults(
            api_wrapper=wrapper, source="news", output_format="list"
        )

        counter = Counter()

        # Run all crawler tasks in parallel using the run() API
        futures = []
        for topic in topics:
            items = search.invoke(topic)
            for item in items:
                future = web_crawler.run(WebPage(url=item["link"]), informer=counter)
                futures.append(future)

        # Wait for all tasks to complete
        wait(futures)

        return counter.succeed
    except Exception as e:
        logger.error(f"Error in web_search: {e}")
        return 0


@function_tool
async def collect_data(topic: str) -> list[str]:
    """
    Collect the necessary information from the vector database based on the topic.
    The information will be returned as a list of strings.

    Args:
        topic: the topic to collect the information from the vector database

    Returns:
        list[str]: the list of contents from the vector database
    """

    embedding_client = EmbeddingClient()
    vector = embedding_client.embed(topic)

    db_client = qdrant_client.QdrantClient(host="qdrant", port=6333)

    results = db_client.query_points(collection_name="sra", query=vector, limit=3)
    payloads = [result.payload for result in results.points]

    return payloads


ins = service.FlameInstance()

sys_prompt = """
You are a writer agent for research; you will write the research paper based on the research topics and the necessary information from the tools.
As a writer, you should follow the following rules:
    1. You should write the research paper in a professional and academic style.
    2. The research paper should also include a prediction section, which should be based on the necessary information from the tools.
    3. Try to use python script to do the calculation and prediction.
    4. You should use the necessary information from the vector database to write the research paper.
    5. The research paper should be written in a concise and clear manner.
    6. The research paper should be written in a logical and coherent manner.
    7. The research paper should be written in a consistent manner.
    8. The research paper should be written in a markdown format.
    9. The output should only include the research paper, no other text or explanation.
"""

agent = Agent(
    name="sra-openai",
    model="deepseek-chat",
    tools=[run_script, collect_data, web_search],
    instructions=sys_prompt,
)

client = qdrant_client.QdrantClient(host="qdrant", port=6333)
if not client.collection_exists("sra"):
    client.create_collection(
        collection_name="sra",
        vectors_config=VectorParams(size=1024, distance=Distance.COSINE),
    )


@ins.entrypoint
async def sra(q: Question) -> Answer:
    result = await Runner.run(agent, q.topic)

    return Answer(answer=result.final_output)


if __name__ == "__main__":
    logging.basicConfig(
        level=logging.DEBUG,
        format="%(asctime)s - %(name)s - %(levelname)s - %(message)s",
    )
    ins.run()
