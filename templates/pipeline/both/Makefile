# Makefile for {{project}}: `make` runs the targets pipeline in _targets.R.

.PHONY: all clean deep-clean sync-mtimes

all:
	Rscript -e 'targets::tar_make()'

# Remove targets' metadata so the next `make` re-checks the whole pipeline;
# stored target results are kept.
clean:
	Rscript -e 'targets::tar_destroy(destroy = "meta", ask = FALSE)'

# Remove the entire _targets/ data store: every target rebuilds from scratch.
deep-clean:
	Rscript -e 'targets::tar_destroy(destroy = "all", ask = FALSE)'

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
