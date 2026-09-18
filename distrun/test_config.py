"""Focused unit coverage for shell-style expansion's easily confused operators."""

import pytest

from distrun import DistrunError
from distrun.config import expand


@pytest.mark.parametrize(
    ("source", "expected"),
    [
        ("$SET/${SET}", "value/value"),
        ("${MISSING:-fallback}", "fallback"),
        ("${EMPTY:-fallback}", "fallback"),
        ("${EMPTY-fallback}", ""),
        ("${SET:+alternate}", "alternate"),
        ("${EMPTY+alternate}", "alternate"),
        ("${EMPTY:+alternate}", ""),
        ("$${SET}", "${SET}"),
        ("${LITERAL}", "${SET}"),
    ],
)
def test_expansion_operator_semantics(source, expected):
    assert expand(source, {"SET": "value", "EMPTY": "", "LITERAL": "${SET}"}) == expected


@pytest.mark.parametrize("source", ["${MISSING}", "${EMPTY:?required}"])
def test_expansion_requires_missing_or_nonempty_value(source):
    with pytest.raises(DistrunError):
        expand(source, {"EMPTY": ""})
