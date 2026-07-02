#!/usr/bin/env bash
#
# Slurm batch job for {{project}}. Submit from the project root:
#
#     sbatch scripts/slurm_job.sh
#
# See docs/slurm.md for monitoring, logs, and resource tuning.

#SBATCH --job-name={{project}}
#SBATCH --output=logs/%x-%j.out
#SBATCH --error=logs/%x-%j.err
#SBATCH --time=01:00:00
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=4
#SBATCH --mem=8G
# Uncomment (single #) to get mail when the job ends or fails:
##SBATCH --mail-type=END,FAIL
##SBATCH --mail-user=your-sunet@stanford.edu

set -euo pipefail

# Load what the job needs; names vary by cluster (`module avail apptainer`).
module load apptainer

# Run the pipeline inside the project's Apptainer image. Build the image
# first if you have not: apptainer build container.sif container.def
# Edit the command below to run your project's entry point.
apptainer exec container.sif {{run_command}}
