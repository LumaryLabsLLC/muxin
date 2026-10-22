# Muxin's box image: Debian with git, the GitHub CLI, Node.js, common build
# tools and the agent CLIs. Built on each machine as its own user (UID/GID),
# so files in a box's folders stay theirs.
FROM node:22-bookworm
ARG UID=1000
ARG GID=1000
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends sudo gh ripgrep fd-find jq less unzip zip tree file openssh-client rsync python3-venv python3-pip \
 && rm -rf /var/lib/apt/lists/* \
 && ln -s /usr/bin/fdfind /usr/local/bin/fd
RUN userdel -r node 2>/dev/null; \
    getent group "$GID" >/dev/null || groupadd -g "$GID" box; \
    useradd -m -o -u "$UID" -g "$GID" -s /bin/bash box \
 && echo 'box ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/box && chmod 440 /etc/sudoers.d/box \
 && mkdir -p /opt/agents && chown "$UID:$GID" /opt/agents
ENV NPM_CONFIG_PREFIX=/opt/agents PATH=/opt/agents/bin:$PATH
USER box
RUN npm install -g @anthropic-ai/claude-code @openai/codex @sourcegraph/amp opencode-ai @google/gemini-cli @mariozechner/pi-coding-agent \
 && npm cache clean --force
USER root
