import sys

expected = tuple(map(int, sys.argv[1:]))
assert sys.version_info[: len(expected)] == expected, sys.version
assert sum(range(6)) == 15
