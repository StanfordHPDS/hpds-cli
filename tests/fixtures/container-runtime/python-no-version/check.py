import sys

minimum = tuple(map(int, sys.argv[1:3]))
maximum = tuple(map(int, sys.argv[3:5]))
assert minimum <= sys.version_info[:2] < maximum, sys.version
assert sum(range(6)) == 15
