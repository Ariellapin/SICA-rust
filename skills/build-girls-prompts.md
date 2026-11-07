---
name: build-girls-prompts
description: Generate 111 girl prompts from girls_inst.md and replace <random> with the 11-pose block via build_prompts.ps1
---
Steps (working directory: C:\Pograms\StabilityMatrix-win-x64\Data\Packages\SwarmUI\Data\Wildcards):

1. Read girls_inst.md — it defines the girl block rules, the pose distribution, the environment options, and the template.
2. Generate 111 main prompts, one per line, using the template:
   real raw analog photo, lomographic, realistic, <girl>, <random>, <environment/lighting tail>
   - <girl>: a unique description per prompt following the girl block (ethnicity, hair, age 18–22, facial features, body). At least 26 of the 111 must be black-haired. Never include poses in the girl description.
   - <random>: the literal placeholder, exactly one per line. The script replaces it.
   - Environment: pick one option from the environment block and write it inline at the end.
   - Every line must be a single line, comma-separated, no full sentences.
3. Write the 111 lines to main_prompts.txt (overwrite).
4. Run the replacement script:
   run-pwsh 
