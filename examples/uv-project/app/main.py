"""Demo B 夹具的示例模块：只求可导入、可测试。"""


def greet(name: str) -> str:
    """向 name 打招呼。"""
    return f"Hello, {name}!"


if __name__ == "__main__":
    print(greet("ignis"))
