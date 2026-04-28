"""
Copyright 2025 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
"""

import flamepy


def test_register_application():
    flamepy.register_application(
        "flmtestapp",
        flamepy.ApplicationAttributes(),
    )

    app = flamepy.get_application("flmtestapp")
    assert app.name == "flmtestapp"
    assert app.state == flamepy.ApplicationState.ENABLED

    flamepy.unregister_application("flmtestapp")


def test_list_application():
    apps = flamepy.list_applications()
    assert len(apps) == 3

    for app in apps:
        assert app.name in ["flmexec", "flmping", "flmrun"]
        assert app.state == flamepy.ApplicationState.ENABLED


def test_application_with_url():
    """Test registering and retrieving an application with URL field."""
    test_url = "file:///opt/test-package.whl"

    # Register application with URL
    flamepy.register_application(
        "flmtestapp-url",
        flamepy.ApplicationAttributes(
            url=test_url,
            description="Test application with URL",
        ),
    )

    # Retrieve and verify URL field
    app = flamepy.get_application("flmtestapp-url")
    assert app.name == "flmtestapp-url"
    assert app.state == flamepy.ApplicationState.ENABLED
    assert app.url == test_url, f"Expected url to be '{test_url}', got '{app.url}'"
    # Note: description field persistence is a separate pre-existing issue

    # Clean up
    flamepy.unregister_application("flmtestapp-url")


def test_application_without_url():
    """Test that application without URL field works correctly (backward compatibility)."""
    # Register application without URL
    flamepy.register_application(
        "flmtestapp-no-url",
        flamepy.ApplicationAttributes(
            description="Test application without URL",
        ),
    )

    # Retrieve and verify URL field is None
    app = flamepy.get_application("flmtestapp-no-url")
    assert app.name == "flmtestapp-no-url"
    assert app.state == flamepy.ApplicationState.ENABLED
    assert app.url is None, f"Expected url to be None, got '{app.url}'"
    # Note: description field persistence is a separate pre-existing issue

    # Clean up
    flamepy.unregister_application("flmtestapp-no-url")


def test_register_application_no_shim():
    """Test registering an application without explicit shim field.

    This test verifies that applications can be registered without explicitly
    specifying a shim, and the application will default to Host shim.
    """
    app_name = "flmtestapp-no-shim"

    # Register application without explicit shim (defaults to Host)
    flamepy.register_application(
        app_name,
        flamepy.ApplicationAttributes(
            description="Test application without explicit shim field",
            command="/usr/bin/python3",
            arguments=["-c", "print('hello')"],
        ),
    )

    # Retrieve and verify the application
    app = flamepy.get_application(app_name)
    assert app is not None, f"Application '{app_name}' should exist"
    assert app.name == app_name
    assert app.state == flamepy.ApplicationState.ENABLED
    assert app.command == "/usr/bin/python3"
    assert app.arguments == ["-c", "print('hello')"]

    # Verify that Application has shim attribute and defaults to Host
    assert hasattr(app, "shim"), "Application should have 'shim' attribute"
    assert app.shim == flamepy.Shim.HOST, f"Default shim should be HOST, got {app.shim}"

    # Clean up
    flamepy.unregister_application(app_name)

    # Verify cleanup
    app_after_cleanup = flamepy.get_application(app_name)
    assert app_after_cleanup is None, f"Application '{app_name}' should be unregistered"
