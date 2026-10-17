"""BM25 retrieval over an in-memory corpus."""


class Bm25Index:
    def __init__(self, corpus: list[str]):
        import bm25s

        self._corpus = corpus
        self._index = bm25s.BM25()
        self._index.index(bm25s.tokenize(corpus, stopwords="en", show_progress=False), show_progress=False)

    def search(self, queries: list[str], top_k: int = 3) -> list[list[str]]:
        import bm25s

        if not queries:
            return []
        tokens = bm25s.tokenize(queries, stopwords="en", show_progress=False)
        k = min(top_k, len(self._corpus))
        documents, _ = self._index.retrieve(tokens, corpus=self._corpus, k=k, show_progress=False)
        return [list(row) for row in documents]
