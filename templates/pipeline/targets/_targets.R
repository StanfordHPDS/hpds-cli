# _targets.R: targets pipeline for {{project}}.
#
# Run the pipeline: targets::tar_make()
# Inspect the graph: targets::tar_visnetwork()
#
# renv note: targets and tarchetypes must live in the project library so the
# pipeline is reproducible. Install and record them with:
#   renv::install(c("targets", "tarchetypes"))
#   renv::snapshot()

library(targets)
library(tarchetypes)

tar_option_set(
  # Packages your targets need at runtime, e.g.:
  # packages = c("dplyr", "ggplot2")
)

# Starter pipeline: replace these name = command pairs with your project's
# real steps. Each target reruns only when its code or upstream data change.
tar_plan(
  raw_data = data.frame(x = 1:10, y = (1:10)^2),
  reg = lm(y ~ x, data = raw_data)

  # tar_plan() also accepts target factories as unnamed arguments, e.g.:
  # tar_quarto(report, "report.qmd")
)
