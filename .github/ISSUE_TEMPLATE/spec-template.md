---
name: "Especificação Funcional (spec.md)"
about: "Especificação funcional de uma Feature. Gerado automaticamente ao mover para a coluna 📋 Spec."
title: "[FEATURE] "
labels: "[FEATURE]"
assignees: ""
---

# Visão Geral

- **Objetivo:** <!-- O que esta Feature entrega e qual problema resolve? -->
- **Personas:** <!-- Quais usuários são afetados? (ex.: Garçom, Cozinha) -->
- **Critérios de Sucesso:** <!-- Resultados mensuráveis -->

# Regras de Negócio

<!-- Liste as regras que governam o comportamento desta Feature -->

# Fluxos

## Fluxo Principal (Happy Path)

<!-- Passo a passo do caminho feliz -->

## Fluxos Alternativos

<!-- Variações do fluxo principal -->

## Cenários de Erro

<!-- O que acontece quando algo dá errado? Como o sistema se comporta? -->

# Critérios de Aceite

<!-- Use o formato Gherkin (Given/When/Then). Um cenário por critério. -->

```gherkin
Feature: [Nome da Feature]
  Scenario: [Título do cenário]
    Given [pré-condição]
    When [ação]
    Then [resultado esperado]
```

# Dependências

- **Internas:** <!-- Serviços/APIs dentro do sistema -->
- **Externas:** <!-- Sistemas de terceiros -->

# Requisitos Não-Funcionais

- **Performance:** <!-- ex.: tempo de resposta < 200ms -->
- **Segurança:** <!-- ex.: RBAC obrigatório -->
- **Usabilidade:** <!-- ex.: responsivo em mobile -->
