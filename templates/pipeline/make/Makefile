# Makefile for {{project}}.
#
# Starter targets: wire your pipeline's real outputs into `all`, then keep
# `clean`/`deep-clean` honest about what is regenerable.

.PHONY: all clean deep-clean sync-mtimes

# Add your pipeline's final outputs as prerequisites of `all`, e.g.:
#   all: output/report.html
all:
	@echo "add your pipeline outputs as prerequisites of 'all'"

# Remove regenerable intermediates; keep raw data and final outputs.
clean:
	rm -rf tmp

# Also remove final outputs so the next `make` rebuilds everything.
deep-clean: clean
	rm -rf output

# A fresh clone stamps every file with checkout time, so `make` thinks
# everything is out of date. Reset each tracked file's mtime to its last
# commit time. Note: needs GNU touch (on macOS: brew install coreutils,
# then replace `touch` with `gtouch`).
sync-mtimes:
	@git ls-files | while read -r file; do \
		stamp=$$(git log -1 --format=%cI -- "$$file"); \
		if test -n "$$stamp"; then touch -d "$$stamp" "$$file"; fi; \
	done

# Optional execution examples
#
# These targets are examples only. Uncomment and adapt the ones this project
# uses after generating the supporting files with `hpds use container` and
# `hpds use slurm`. Recipe lines retain their required leading tab.
#
# .PHONY: docker-build docker-run apptainer-build apptainer-run slurm-submit
#
# docker-build:
#	docker build --tag {{project}} .
#
# docker-run:
#	docker run --rm {{project}} make
#
# apptainer-build:
#	apptainer build container.sif container.def
#
# apptainer-run:
#	apptainer exec container.sif make
#
# slurm-submit:
#	sbatch scripts/slurm_job.sh
