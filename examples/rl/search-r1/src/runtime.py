"""The Flame application every service in this package belongs to.

`flamepy.app` has one application per process, and `@app.service` needs it to exist at
decoration time, so the services import `app` from here. The entry points choose the
application and the executor counts through the environment before importing the services:

    SR1_APP=sr1-train SR1_ROLLOUTS=8 SR1_TRAINERS=4 python3 src/train.py
"""

import os

import flamepy.app as app

PYTHON_VERSION = "3.12"
NAME = os.environ.get("SR1_APP", "sr1-train")
# Checkpoints must outlive the training application. The cache deletes the
# objects of an application that is destroyed, unregistered or missing, so they live under
# a placeholder application that is registered once and never run or unregistered.
CHECKPOINT_PREFIX_APP = "sr1-models"
ROLLOUTS = int(os.environ.get("SR1_ROLLOUTS", "4"))
TRAINERS = int(os.environ.get("SR1_TRAINERS", "1"))

# Dependencies come from the pyproject.toml of the component being run.
app.init(NAME, python_version=PYTHON_VERSION)

__all__ = ["app", "CHECKPOINT_PREFIX_APP", "NAME", "ROLLOUTS", "TRAINERS"]
