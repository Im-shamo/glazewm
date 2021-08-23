﻿using System;
using System.Collections.Generic;
using System.Linq;

namespace LarsWM.Domain.Containers
{
  public class Container
  {
    public virtual int Height { get; set; }
    public virtual int Width { get; set; }
    public virtual int X { get; set; }
    public virtual int Y { get; set; }
    public double SizePercentage { get; set; }
    public Container Parent { get; set; } = null;
    public List<Container> Children { get; set; } = new List<Container>();
    public Container LastFocusedContainer { get; set; } = null;

    public List<Container> SelfAndSiblings => Parent.Children;

    public IEnumerable<Container> Siblings
    {
      get
      {
        return Parent.Children.Where(child => child != this);
      }
    }

    /// <summary>
    /// Index of this container amongst its siblings.
    /// </summary>
    public int Index => Parent.Children.IndexOf(this);

    /// <summary>
    /// Get the element at the bottom of the focus stack.
    /// </summary>
    public Container LastFocusedTail
    {
      get
      {
        var tail = LastFocusedContainer;

        while (tail?.LastFocusedContainer != null)
          tail = tail.LastFocusedContainer;

        return tail;
      }
    }

    // TODO: Rename to SelfAndDescendants and change to getter.
    public IEnumerable<Container> Flatten()
    {
      return new[] { this }.Concat(Children.SelectMany(x => x.Flatten()));
    }

    public Container AddChild(Container container)
    {
      Children.Add(container);
      container.Parent = this;
      return container;
    }

    public Container[] AddChildren(params Container[] containers)
    {
      return containers.Select(AddChild).ToArray();
    }

    public bool RemoveChild(Container node)
    {
      node.Parent = null;
      return Children.Remove(node);
    }

    // Not sure if needed.
    public void Traverse(Action<Container> action)
    {
      action(this);
      foreach (var child in Children)
        child.Traverse(action);
    }

    public bool HasChildren()
    {
      return Children.Count > 0;
    }
  }
}
