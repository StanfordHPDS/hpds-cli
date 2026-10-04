stopifnot(nzchar(digest::digest(1:5)))
stopifnot(vendored::fixture_value() == 42L)
