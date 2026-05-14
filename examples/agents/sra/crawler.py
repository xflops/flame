import markitdown
import qdrant_client
from flamepy import service
import requests
import io
import uuid

from qdrant_client.models import VectorParams, Distance, PointStruct

from apis import WebPage, Answer
from embed import EmbeddingClient

ins = service.FlameInstance()

headers = {"User-Agent": "Xflops Crawler 1.0", "From": "support@xflops.io"}


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

        client.upsert(
            collection_name="sra",
            points=[
                PointStruct(
                    id=f"{uuid.uuid4()}",
                    vector=vector,
                    payload={
                        "url": wp.url,
                        "chunk": chunk,
                        "content": result[chunk : chunk + chunk_size],
                    },
                )
            ],
        )

    return Answer(answer=f"Crawled {wp.url}")


if __name__ == "__main__":
    ins.run()
