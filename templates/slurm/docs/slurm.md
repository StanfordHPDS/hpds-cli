# Running {{project}} on Slurm

The batch script lives at `scripts/slurm_job.sh`. It runs the project's
pipeline inside the Apptainer image (`container.sif`), so build the image
before the first submission:

```bash
apptainer build container.sif container.def
```

## Submitting

From the project root:

```bash
sbatch scripts/slurm_job.sh
```

Slurm prints a job id on submission. Output and errors land in `logs/`
(one `.out` and one `.err` file per job, named by job name and id).

## Monitoring

```bash
squeue -u "$USER"          # your queued and running jobs
sacct -j <jobid>           # accounting info after the job finishes
scancel <jobid>            # cancel a job
```

## Adjusting resources

Edit the `#SBATCH` lines at the top of `scripts/slurm_job.sh`: `--time`,
`--cpus-per-task`, and `--mem` are the ones you will change most often.
To get mail when a job ends or fails, uncomment the `--mail-type` and
`--mail-user` lines (change `##SBATCH` to `#SBATCH`) and set your address.
