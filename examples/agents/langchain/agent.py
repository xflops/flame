from flamepy import service

from langchain_deepseek import ChatDeepSeek
from langchain.agents import create_agent
from langchain.messages import HumanMessage, SystemMessage

from apis import Question, Answer, SysPrompt

ins = service.FlameInstance()

llm = ChatDeepSeek(
    model="deepseek-chat",
    temperature=0,
    max_tokens=None,
    timeout=None,
    max_retries=2,
)

agent = create_agent(
    model=llm,
)


@ins.entrypoint
def my_agent(q: Question) -> Answer:
    messages = []

    cxt = ins.context()
    if cxt is not None and isinstance(cxt, SysPrompt):
        messages.append(SystemMessage(content=cxt.prompt))

    messages.append(HumanMessage(content=q.question))

    output = agent.invoke({"messages": messages})

    aimsgs = output["messages"][-1]

    return Answer(answer=aimsgs.content)


if __name__ == "__main__":
    ins.run()
