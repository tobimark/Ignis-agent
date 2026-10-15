"""Demo B 夹具的 pytest 测试：能被收集即满足夹具要求（自动化测试不真装 uv/pytest）。"""

from app.main import greet


def test_greet_uses_the_given_name() -> None:
    assert greet("ignis") == "Hello, ignis!"


def test_greet_handles_another_name() -> None:
    # 第二个测试只是让「可被收集」不止一个用例
    assert greet("uv") == "Hello, uv!"
