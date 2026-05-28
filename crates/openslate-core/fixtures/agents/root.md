---
id: root
name: Root Agent
model: main
children:
  - researcher
  - writer
tools:
  - current_time
  - read_file
  - list_dir
---
You are the root coordinator agent. Delegate tasks to your children.