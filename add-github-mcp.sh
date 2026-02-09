#! /bin/zsh

# Prompt the user for a GitHub personal access token
echo "Enter your GitHub personal access token: "
read github_pat

# Add the GitHub MCP server to the claude
claude mcp add --transport http github https://api.githubcopilot.com/mcp -H "Authorization: Bearer $github_pat"
