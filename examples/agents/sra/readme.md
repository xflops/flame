# Simple Research Agent (SRA)

In the rapidly evolving landscape of AI and automation, tool-augmented agents are emerging as powerful solutions for complex task orchestration. Today, we'll explore the **Simple Research Agent (SRA)**, a compelling example built with Flame that demonstrates how an intelligent agent can leverage specialized tools to automate research workflows.

## What is the Simple Research Agent (SRA)?

The SRA is a streamlined, tool-augmented agent system designed to automate research tasks. It leverages advanced language models (DeepSeek) through OpenAI-compatible APIs with the Flame `agents` library and sophisticated tool orchestration to efficiently gather information from the web, process it through a vector database, and generate comprehensive research reports.

At its core, SRA embodies the principle of **intelligent tool orchestration** - a single agent coordinates multiple specialized tools and services, working together to achieve a common goal: producing high-quality research reports on any given topic.

## The Two-Component Architecture

The SRA system consists of two main components working together:

### 1. Research Agent (`sra.py`) - The Orchestrator and Writer

The Research Agent is the heart of the SRA system, combining orchestration, data collection, and report generation into a unified agent. It serves as:

- **Research Coordinator**: Interprets research topics and determines investigation scope
- **Information Gatherer**: Searches the web and manages the crawling process
- **Data Analyst**: Executes Python scripts for computational analysis
- **Report Generator**: Synthesizes collected information into comprehensive research reports

```python
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
```

The agent is built using the Flame `agents` library with DeepSeek's chat model through an OpenAI-compatible API, equipped with three powerful tools:

#### Tool 1: `web_search` - Web Discovery and Crawling
Searches the web using DuckDuckGo and orchestrates parallel crawling operations:
- Searches for recent news and articles (up to 20 results per topic)
- Focuses on current information (filtered by day)
- Invokes the crawler service asynchronously for each URL
- Tracks crawling progress with a `Counter` class that monitors successful, failed, and error states
- Returns the number of successfully crawled URLs
- Includes error handling to gracefully handle search and crawling failures

```python
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
            web_crawler = await flamepy.create_session("crawler")

        wrapper = DuckDuckGoSearchAPIWrapper(time="d", max_results=20)
        search = DuckDuckGoSearchResults(api_wrapper=wrapper, source="news", output_format="list")

        counter = Counter()

        tasks = []
        for topic in topics:
            items = search.invoke(topic)
            for item in items:
                task = web_crawler.invoke(WebPage(url=item["link"]), informer=counter)
                tasks.append(task)

        await asyncio.gather(*tasks)

        return counter.succeed
    except Exception as e:
        logger.error(f"Error in web_search: {e}")
        return 0
```

#### Tool 2: `collect_data` - Vector Database Retrieval
Queries the vector database using semantic search:
- Embeds the search topic using the embedding service
- Retrieves the most relevant information chunks (top 3 results)
- Leverages cosine similarity for intelligent content matching
- Returns content payloads as a list of dictionaries with URLs, chunk indices, and text content

```python
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
```

#### Tool 3: `run_script` - Computational Analysis
Integrates with `flmexec` for secure Python script execution:
- Supports PEP 723 inline script metadata for dependency declaration
- Executes code in a sandboxed environment via `uv run`
- Enables data analysis, calculations, and predictions
- Returns script stdout for incorporation into reports
- Reuses the script_runner session for efficiency

```python
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
        script_runner = await flamepy.create_session("flmexec")

    output = await script_runner.invoke(Script(language="python", code=code))

    return output.decode("utf-8")
```

#### Helper Class: `Counter` - Task Progress Tracking
A custom `TaskInformer` that monitors the progress of parallel crawling operations:
- Tracks the number of successfully completed, failed, and error tasks
- Implements callbacks for task updates and error handling
- Provides real-time visibility into the crawling process

```python
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
```

### 2. Crawler Service (`crawler.py`) - The Web Content Processor

The Crawler Service is a Flame application that processes individual web pages:
- Downloads web page content using proper headers (identifies as "Xflops Crawler 1.0")
- Converts HTML to clean markdown using MarkItDown
- Ensures vector database collection exists before processing
- Chunks content into manageable pieces (up to 1024 bytes per chunk)
- Generates embeddings for each chunk via the embedding service
- Stores chunks with metadata (URL, chunk index, content) in Qdrant vector database
- Returns a confirmation message upon successful crawling

```python
@ins.entrypoint
def crawler(wp: WebPage) -> Answer:
    """
    Crawl the web and persist the content of the web page to the vector database.
    Return the content of the web page.

    Args:
        wp: WebPage object containing the url to crawl
    """
    text = requests.get(wp.url, headers=headers).text
    
    md = markitdown.MarkItDown()
    stream = io.BytesIO(text.encode("utf-8"))
    result = md.convert(stream).text_content
    
    client = qdrant_client.QdrantClient(host="qdrant", port=6333)
    if not client.collection_exists("sra"):
        client.create_collection(
            collection_name="sra",
            vectors_config=VectorParams(size=1024, distance=Distance.COSINE),
        )
    
    chunk_size = min(1024, len(result))
    
    embedding_client = EmbeddingClient()
    
    for chunk in range(0, len(result), chunk_size):
        chunk_end = min(chunk + chunk_size, len(result))
        vector = embedding_client.embed(result[chunk:chunk_end])
        
        client.upsert(collection_name="sra",
                      points=[
                          PointStruct(id=f"{uuid.uuid4()}",
                                      vector=vector,
                                      payload={
                                          "url": wp.url,
                                          "chunk": chunk,
                                          "content": result[chunk:chunk + chunk_size]
                                      })
                      ])
    
    return Answer(answer=f"Crawled {wp.url}")
```

## Deployment Configuration

The SRA system is deployed using Flame's application manifest format (`sra.yaml`):

```yaml
---
metadata:
  name: crawler
spec:
  working_directory: /opt/examples/agents/sra/
  labels:
    - Tool
  environments:
    SILICONFLOW_API_KEY: sk-xxxxxxxxxxxxxxxxx
  command: /usr/bin/uv
  arguments:
    - run
    - -n
    - crawler.py
    - apis.py

---
metadata:
  name: sra
spec:
  working_directory: /opt/examples/agents/sra/
  max_instances: 1
  labels:
    - Agent
  environments:
    DEEPSEEK_API_KEY: sk-xxxxxxxxxxxxxxxxx
    SILICONFLOW_API_KEY: sk-xxxxxxxxxxxxxxxxx
  command: /usr/bin/uv
  arguments:
    - run
    - -n
    - sra.py
    - apis.py
```

Key configuration details:
- **Crawler**: Deployed as a Tool, can scale to multiple instances for parallel processing
- **SRA Agent**: Limited to 1 instance (`max_instances: 1`) to maintain consistency
- **Environment Variables**: Requires API keys for DeepSeek (LLM) and SiliconFlow (embeddings)
- **Runtime**: Uses `uv run` for dependency management and execution

## Project Dependencies

The SRA system uses modern Python tooling with `uv` for dependency management (`pyproject.toml`):

```toml
[project]
name = "sra"
version = "0.1.0"
description = "Simple Research Agent by Flame"
readme = "README.md"
requires-python = ">=3.12"
dependencies = [
  "flamepy",              # Flame SDK for agent framework
  "ddgs",                 # DuckDuckGo search
  "langchain",            # LangChain framework (for utilities)
  "langgraph",            # LangGraph (for utilities)
  "langchain-deepseek",   # DeepSeek LLM integration
  "langchain-community",  # Community tools (DuckDuckGo search)
  "qdrant-client>=1.14.1", # Vector database client
  "requests>=2.32.3",     # HTTP requests
  "markitdown",           # HTML to Markdown conversion
  "python-dotenv",        # Environment variable management
  "pytest",               # Testing framework
]

[tool.uv.sources]
flamepy = { path = "/usr/local/flame/sdk/python" }
```

**Note**: The SRA uses the Flame `agents` library (a custom agent framework) with OpenAI-compatible APIs for agent orchestration, while LangChain is primarily used for utility functions like DuckDuckGo search integration.

## Agent Configuration and Initialization

The SRA agent is configured using the Flame `agents` library with DeepSeek integration:

```python
from openai import AsyncOpenAI
from agents import Agent, Runner, set_default_openai_client, set_tracing_disabled, enable_verbose_stdout_logging, set_default_openai_api

# Set up DeepSeek client as OpenAI-compatible API
ds_client = AsyncOpenAI(base_url="https://api.deepseek.com", api_key=os.getenv("DEEPSEEK_API_KEY"))
set_default_openai_client(ds_client)
set_tracing_disabled(True)
enable_verbose_stdout_logging()
set_default_openai_api("chat_completions")

# Create the agent
agent = Agent(
    name="sra-openai",
    model="deepseek-chat",
    tools=[run_script, collect_data, web_search],
    instructions=sys_prompt,
)

# Initialize vector database
client = qdrant_client.QdrantClient(host="qdrant", port=6333)
if not client.collection_exists("sra"):
    client.create_collection(
        collection_name="sra",
        vectors_config=VectorParams(size=1024, distance=Distance.COSINE),
    )

# Define the entrypoint
@ins.entrypoint
async def sra(q: Question) -> Answer:
    result = await Runner.run(agent, q.topic)
    return Answer(answer=result.final_output)
```

Key configuration elements:
- **OpenAI Compatibility**: Uses `AsyncOpenAI` client with DeepSeek's base URL
- **Agent Setup**: Creates an `Agent` instance with model, tools, and instructions
- **Execution**: Uses `Runner.run()` for agent execution instead of streaming
- **Logging**: Verbose stdout logging enabled for debugging
- **Tracing**: Disabled for performance

## The Supporting Infrastructure

### Vector Database Integration
The system uses Qdrant as its vector database, configured with:
- 1024-dimensional vectors using cosine similarity
- Automatic collection creation and management (collection name: "sra")
- Efficient semantic search capabilities
- Stores content chunks with metadata (URL, chunk index, content)

### Embedding Service (`embed.py`)
The EmbeddingClient provides text-to-vector conversion using:
- **Provider**: SiliconFlow API (api.siliconflow.cn)
- **Model**: Qwen/Qwen3-Embedding-0.6B
- **Dimensions**: 1024-dimensional embeddings
- **Format**: Float encoding for precise semantic matching

```python
class EmbeddingClient:
    def __init__(self, api_key: str = None, model: str = "Qwen/Qwen3-Embedding-0.6B"):
        self.api_url = "https://api.siliconflow.cn/v1/embeddings"
        # ... initialization
    
    def embed(self, text: str) -> list[float]:
        payload = {
            "model": self.model,
            "input": text,
            "encoding_format": "float",
            "dimensions": 1024
        }
        # Returns 1024-dimensional vector
```

### Security and Isolation
The `flmexec` application provides secure script execution:
- Sandboxed environment for Python scripts
- Uses `uv run` for dependency management
- Supports PEP 723 inline script metadata
- Isolated from the main system to ensure safety

### Session Management
The system leverages Flame's session management for efficient resource utilization:
- Reuses `script_runner` session across multiple script executions
- Reuses `web_crawler` session for parallel crawling operations
- Async operations with `asyncio.gather` for parallel processing

## How to Use the SRA

### Client Usage

The SRA system is invoked through a simple client interface (`client.py`):

```python
import flamepy
import asyncio
from datetime import datetime

from apis import Question, Answer

async def build_research_report():
    sra = await flamepy.create_session("sra")

    topic="Write a report about 2025 Nvidia stock performance and predict the stock price in 2026"

    print(f"Building research report for topic: {topic}")

    output = await sra.invoke(Question(topic=topic))
    answer = Answer.from_json(output)

    report_timestamp = datetime.now().strftime('%Y%m%d_%H%M%S')
    report_name = f"sra_report_{report_timestamp}.md"

    with open(report_name, "w") as f:
        f.write(answer.answer)

    print(f"Research report was saved to {report_name}")

    await sra.close()

if __name__ == "__main__":
    asyncio.run(build_research_report())
```

The client demonstrates:
- **Session Creation**: Establishes a connection to the deployed SRA agent
- **Request/Response Model**: Uses typed `Question` and `Answer` objects
- **Async Operations**: Leverages Python's asyncio for efficient execution
- **File Output**: Automatically saves the generated report to a timestamped file
- **Resource Management**: Properly closes sessions after use

### Running the System

1. **Deploy the applications** using Flame:
   ```bash
   flmctl register -f sra.yaml
   ```

2. **Ensure infrastructure is running**:
   - Qdrant vector database (accessible at `qdrant:6333`)
   - SiliconFlow API access for embeddings
   - DeepSeek API access for LLM
   - `flmexec` service for script execution

3. **Run the client**:
   ```bash
   python client.py
   ```

## The Workflow in Action

Here's how the SRA system operates when given a research topic:

1. **User Input**: User provides a research topic to the SRA agent via `client.py`
   ```python
   sra = await flamepy.create_session("sra")
   output = await sra.invoke(Question(topic="Write a report about 2025 Nvidia stock performance"))
   ```

2. **Agent Planning**: The agent analyzes the topic and determines which tools to use

3. **Web Search and Crawling** (via `web_search` tool):
   - Searches DuckDuckGo for relevant recent news articles (up to 20 per topic)
   - Creates parallel crawling tasks for all discovered URLs
   - Each URL is processed by the crawler service asynchronously
   - Crawler downloads content, converts to markdown, chunks it, and embeds it
   - All chunks are stored in the Qdrant vector database with metadata
   - Returns count of successfully crawled URLs

4. **Data Collection** (via `collect_data` tool):
   - Agent embeds the research topic to create a query vector
   - Performs semantic search in Qdrant (top 3 most relevant chunks)
   - Retrieves content chunks with source URLs

5. **Computational Analysis** (via `run_script` tool, optional):
   - Agent generates Python scripts for data analysis or predictions
   - Scripts execute in isolated `flmexec` environment
   - Results are incorporated into the research report

6. **Report Generation**:
   - Agent synthesizes all collected information
   - Produces a comprehensive markdown report following academic style guidelines
   - Includes predictions based on data and computational analysis

7. **Delivery**: Final report is returned to the user as an `Answer` object

## Conclusion

The Simple Research Agent showcases the power of tool-augmented AI agents in automating complex, multi-step workflows. By combining the Flame `agents` library with DeepSeek's language model and specialized tools and services, Flame enables the creation of intelligent systems that can tackle real-world research tasks with efficiency and reliability.

Key architectural highlights:
- **Modern Agent Framework**: Uses Flame's custom `agents` library with OpenAI-compatible APIs for flexible model integration
- **Unified Agent**: Single research agent with multiple specialized tools (web search, data collection, script execution)
- **Parallel Processing**: Asynchronous crawling operations with task monitoring for efficient data collection
- **Semantic Search**: Vector database integration for intelligent information retrieval
- **Computational Capabilities**: Secure script execution for data analysis and predictions
- **Modular Design**: Clean separation between agent logic and crawler service
- **Error Handling**: Robust error handling with logging and graceful failure recovery
- **Session Reuse**: Efficient resource utilization through global session management

Whether you're building research tools, content generation systems, or any application requiring AI-powered automation, the SRA example provides a solid foundation for understanding how to architect and implement tool-augmented agent solutions with Flame.

The future of AI lies in intelligent agents that can orchestrate complex workflows using specialized tools - and the SRA is a perfect example of this paradigm in action.

---

*Ready to build your own tool-augmented agent system? Explore the SRA example in the Flame repository and start creating intelligent, automated research workflows today.*
