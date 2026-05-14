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

import argparse
from typing import Optional

from flamepy.service import Session
from apis import MyContext, Question

OPENAI_APP_NAME = "openai-agent"


def main(message: str, ssn_id: Optional[str] = None):
    if ssn_id:
        session = Session(session_id=ssn_id)
    else:
        sys_prompt = """You are a weather forecaster.
        If you are asked to fetch the weather, you should use the fetch_weather tool after confirming the location with the user.
        """
        session = Session(OPENAI_APP_NAME, ctx=MyContext(prompt=sys_prompt))

    print(f"{'=' * 30}")
    print(f"Conversation <{session.id()}>")
    print(f"{'=' * 30}")

    print(f"User: {message}")

    output = session.invoke(Question(question=message))

    print(f"Agent: {output.answer}")

    cxt = session.context()
    print(f"{'=' * 30}")
    print("Session History")
    print(f"{'=' * 30}")
    if getattr(cxt, "messages", None) is not None:
        for msg in cxt.messages:
            print(msg)
    else:
        print("No history!")

    session.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "-s", "--session", type=str, default=None, help="The session to open"
    )
    parser.add_argument(
        "-m",
        "--message",
        type=str,
        required=True,
        help="The message to send to the agent",
    )
    args = parser.parse_args()

    main(args.message, args.session)
