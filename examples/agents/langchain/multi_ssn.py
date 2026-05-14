from flamepy.service import Session
from apis import SysPrompt, Question

LANGCHAIN_AGENT_NAME = "langchain-agent"


def ask_agent():
    weather_sys_prompt = SysPrompt(prompt="You are a weather forecaster.")
    news_sys_prompt = SysPrompt(prompt="You are a news reporter.")

    question = Question(question="Who are you?")

    session = Session(LANGCHAIN_AGENT_NAME, ctx=weather_sys_prompt)
    output = session.invoke(question)
    print(output.answer)
    session.close()

    session = Session(LANGCHAIN_AGENT_NAME, ctx=news_sys_prompt)
    output = session.invoke(question)
    print(output.answer)
    session.close()


if __name__ == "__main__":
    ask_agent()
